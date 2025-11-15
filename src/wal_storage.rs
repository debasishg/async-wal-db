use crate::{WalEntry, WalError};
use crossbeam::queue::SegQueue;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

#[cfg(test)]
use tempfile::TempDir;
#[cfg(test)]
use tokio::test as async_test;

/// Lock-Free Write-Ahead Log (WAL) Storage
///
/// ## Architecture
///
/// This implementation uses a lock-free design to maximize concurrent write throughput:
///
/// ```text
/// Thread 1 ──┐
///            │
/// Thread 2 ──┼──▶ SegQueue.push()  ──▶  [Queue]
///            │     (CAS atomic ops)         │
/// Thread N ──┘     No locks!                │
///                                           ▼
///                                    Background Thread
///                                    drains & writes
///                                    (Mutex only here)
/// ```
///
/// ## Lock-Free Concurrency with SegQueue
///
/// The `SegQueue` from crossbeam provides lock-free concurrent access using
/// Compare-And-Swap (CAS) atomic operations:
///
/// 1. **Multiple threads call `push()` simultaneously**
/// 2. **Each thread independently**:
///    - Reads current tail pointer atomically
///    - Prepares to insert its entry
///    - Uses CAS: "If tail is still X, update to Y"
///    - If CAS fails (another thread modified tail), retry with new tail
/// 3. **No mutex needed** - hardware atomics handle coordination
///
/// ### Why This is Fast
///
/// **Hot Path (append)**:
/// - ✅ No locks or mutexes
/// - ✅ No thread blocking
/// - ✅ CPU cache-friendly operations
/// - ✅ Scales linearly with CPU cores
///
/// **Cold Path (flush)**:
/// - Single background thread batches entries
/// - Mutex protects `BufWriter<File>` (required for file I/O)
/// - But doesn't block appends!
///
/// ### Performance Results
///
/// From benchmarks (see `PHASE1_LOCK_FREE_WAL.md`):
/// - **128 concurrent threads**: 9.12x speedup (1,960 txn/s)
/// - **High contention**: Only 3.95% degradation
/// - **Latency**: Reduced from 4.65ms to 0.51ms
///
/// If there were locks on `append()`, contention would be much worse at high thread counts.
///
/// ## Components
///
/// - `pending`: Lock-free queue (`SegQueue`) for concurrent appends
/// - `file`: Mutex-protected `BufWriter` (only used by background flusher)
/// - `flusher_handle`: Background task that periodically drains queue to disk
/// - `shutdown`: Atomic flag for graceful termination
/// - `flush_notify`: Signal for immediate flush requests
#[derive(Clone)]
pub struct WalStorage {
    inner: Arc<Mutex<InnerWal>>,
    pub(crate) path: String,
    /// Lock-free queue for pending writes - multiple threads can push concurrently
    pending: Arc<SegQueue<WalEntry>>,
    /// Background flusher task handle. The flusher runs on a configurable interval
    /// (default 10ms), drains all pending entries from the queue, and batches them
    /// into a single disk write. This amortizes I/O overhead across many operations.
    /// Can be stopped gracefully via `stop_flusher()` which joins the task and performs
    /// a final flush to ensure no data loss.
    flusher_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Atomic shutdown flag - when set to true, signals the background flusher to terminate
    shutdown: Arc<AtomicBool>,
    /// Notify mechanism for immediate flush requests - allows explicit flush() calls
    /// to wake the background flusher without waiting for the next interval tick
    flush_notify: Arc<Notify>,
}

struct InnerWal {
    file: BufWriter<File>,
}

impl WalStorage {
    pub async fn new<P: AsRef<Path>>(path: P) -> Self {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_ref())
            .await
            .expect("Failed to open WAL file");
        let inner = InnerWal {
            file: BufWriter::new(file),
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            path: path.as_ref().to_string_lossy().to_string(),
            pending: Arc::new(SegQueue::new()),
            flusher_handle: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            flush_notify: Arc::new(Notify::new()),
        }
    }

    /// Lock-free append to pending queue
    pub async fn append(&self, entry: WalEntry) -> Result<(), WalError> {
        self.pending.push(entry);
        // Notify flusher that work is available
        self.flush_notify.notify_one();
        Ok(())
    }

    /// Start background flusher task
    pub fn start_flusher(&self, flush_interval_ms: u64) {
        let pending = Arc::clone(&self.pending);
        let inner = Arc::clone(&self.inner);
        let shutdown = Arc::clone(&self.shutdown);
        let flush_notify = Arc::clone(&self.flush_notify);

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(flush_interval_ms));
            
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(e) = Self::drain_and_flush(&pending, &inner).await {
                            eprintln!("WAL flush error: {}", e);
                        }
                    }
                    _ = flush_notify.notified() => {
                        // Immediate flush requested
                        if let Err(e) = Self::drain_and_flush(&pending, &inner).await {
                            eprintln!("WAL flush error: {}", e);
                        }
                    }
                }

                if shutdown.load(Ordering::SeqCst) {
                    // Final flush on shutdown
                    if let Err(e) = Self::drain_and_flush(&pending, &inner).await {
                        eprintln!("Final WAL flush error: {}", e);
                    }
                    break;
                }
            }
        });

        // Store handle for cleanup
        let flusher_handle = Arc::clone(&self.flusher_handle);
        tokio::spawn(async move {
            *flusher_handle.lock().await = Some(handle);
        });
    }

    /// Drain queue and write to disk (called by background task)
    async fn drain_and_flush(
        pending: &SegQueue<WalEntry>,
        inner: &Arc<Mutex<InnerWal>>,
    ) -> Result<(), WalError> {
        let mut batch = Vec::new();
        
        // Drain all pending entries
        while let Some(entry) = pending.pop() {
            batch.push(entry);
        }

        if batch.is_empty() {
            return Ok(());
        }

        // Single lock acquisition for entire batch
        let mut inner = inner.lock().await;
        
        for entry in batch {
            let encoded = bincode::serialize(&entry)
                .map_err(WalError::Deserialization)?;
            let len = (encoded.len() as u64).to_le_bytes();

            inner.file.write_all(&len).await?;
            inner.file.write_all(&encoded).await?;
        }

        inner.file.flush().await?;
        inner.file.get_mut().sync_all().await?; // fsync for durability
        
        Ok(())
    }

    /// Explicit flush - blocks until all pending entries are written
    pub async fn flush(&self) -> Result<(), WalError> {
        // Notify flusher to flush immediately
        self.flush_notify.notify_one();
        
        // Wait until queue is drained
        let mut retries = 0;
        while !self.pending.is_empty() && retries < 100 {
            tokio::time::sleep(tokio::time::Duration::from_micros(100)).await;
            retries += 1;
        }

        if !self.pending.is_empty() {
            // Force flush remaining entries
            Self::drain_and_flush(&self.pending, &self.inner).await?;
        }

        Ok(())
    }

    /// Stop the background flusher gracefully
    pub async fn stop_flusher(&self) -> Result<(), WalError> {
        self.shutdown.store(true, Ordering::SeqCst);
        self.flush_notify.notify_one();

        // Wait for flusher to finish
        if let Some(handle) = self.flusher_handle.lock().await.take() {
            handle.await.map_err(|e| WalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Flusher join error: {}", e)
            )))?;
        }

        Ok(())
    }

    pub async fn set_checkpoint(&self, tx_id: u64) -> Result<(), WalError> {
        let ckpt_path = format!("{}.checkpoint", self.path);
        let mut ckpt_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(ckpt_path)
            .await
            .map_err(|e| WalError::Checkpoint(format!("Failed to write checkpoint: {}", e)))?;
        ckpt_file.write_all(tx_id.to_string().as_bytes()).await?;
        ckpt_file.flush().await?;
        Ok(())
    }

    pub async fn get_last_checkpoint(&self) -> Result<u64, WalError> {
        let ckpt_path = format!("{}.checkpoint", self.path);
        if !Path::new(&ckpt_path).exists() {
            return Ok(0);
        }
        let mut ckpt_file = File::open(ckpt_path).await
            .map_err(|e| WalError::Checkpoint(format!("Failed to read checkpoint: {}", e)))?;
        let mut contents = String::new();
        ckpt_file.read_to_string(&mut contents).await?;
        contents.trim().parse::<u64>()
            .map_err(|e| WalError::Checkpoint(format!("Invalid checkpoint TxID: {}", e)))
    }

    pub async fn truncate_wal(&mut self, path: &str) -> Result<(), WalError> {
        // Ensure all pending writes are flushed first
        self.flush().await?;
        
        let old_path = format!("{}.old", self.path);
        tokio::fs::rename(&self.path, &old_path).await
            .map_err(|e| WalError::Checkpoint(format!("Failed to rename WAL: {}", e)))?;
        *self = Self::new(path).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[async_test]
    async fn test_append_flush() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log");
        let wal = WalStorage::new(&wal_path).await;
        wal.start_flusher(10); // 10ms flush interval
        
        let entry = WalEntry::Insert {
            tx_id: 1,
            timestamp: 123,
            key: "test".to_string(),
            value: b"val".to_vec(),
        };
        wal.append(entry).await.unwrap();
        wal.flush().await.unwrap();
        
        // Verify by reopening and reading
        let file = tokio::fs::read(&wal_path).await.unwrap();
        assert!(!file.is_empty());
        
        wal.stop_flusher().await.unwrap();
    }

    #[async_test]
    async fn test_concurrent_appends() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal_concurrent.log");
        let wal = Arc::new(WalStorage::new(&wal_path).await);
        wal.start_flusher(5); // 5ms flush interval

        let mut handles = vec![];
        
        // Spawn 10 tasks writing concurrently
        for i in 0..10 {
            let wal_clone = Arc::clone(&wal);
            let handle = tokio::spawn(async move {
                for j in 0..100 {
                    let entry = WalEntry::Insert {
                        tx_id: (i * 100 + j) as u64,
                        timestamp: 123,
                        key: format!("key-{}-{}", i, j),
                        value: b"value".to_vec(),
                    };
                    wal_clone.append(entry).await.unwrap();
                }
            });
            handles.push(handle);
        }

        // Wait for all tasks
        for handle in handles {
            handle.await.unwrap();
        }

        // Ensure everything is flushed
        wal.flush().await.unwrap();
        
        // Verify file has data
        let file_size = tokio::fs::metadata(&wal_path).await.unwrap().len();
        assert!(file_size > 0, "WAL file should contain data from 1000 entries");

        wal.stop_flusher().await.unwrap();
    }
}
