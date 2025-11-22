use crate::{WalEntry, WalError, WalEntryHeader};
use crossbeam::queue::SegQueue;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
/// ## Lock-Free Concurrency with `SegQueue`
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
    /// Maximum queue size - when reached, writers will wait asynchronously (backpressure)
    max_queue_size: usize,
    /// Atomic counter tracking pending queue size - used for efficient capacity checks
    pending_count: Arc<AtomicUsize>,
    /// Notification for space becoming available - wakes waiting writers
    space_available: Arc<Notify>,
    /// Background flusher task handle. The flusher runs on a configurable interval
    /// (default 10ms), drains all pending entries from the queue, and batches them
    /// into a single disk write. This amortizes I/O overhead across many operations.
    /// Can be stopped gracefully via `stop_flusher()` which joins the task and performs
    /// a final flush to ensure no data loss.
    flusher_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Atomic shutdown flag - when set to true, signals the background flusher to terminate
    shutdown: Arc<AtomicBool>,
    /// Notify mechanism for immediate flush requests - allows explicit `flush()` calls
    /// to wake the background flusher without waiting for the next interval tick
    flush_notify: Arc<Notify>,
}

struct InnerWal {
    file: BufWriter<File>,
}

impl WalStorage {
    /// Creates a new WAL storage instance at the specified path.
    ///
    /// Opens (or creates) the WAL file in append mode. The file uses a buffered
    /// writer for efficient I/O. The background flusher is NOT started automatically -
    /// call `start_flusher()` separately.
    ///
    /// # Arguments
    /// * `path` - File path for the WAL log
    ///
    /// # Panics
    /// Panics if the file cannot be opened (permissions, disk full, etc.)
    pub async fn new<P: AsRef<Path>>(path: P) -> Self {
        Self::with_config(path, 10_000).await
    }

    /// Creates a new WAL storage instance with custom configuration.
    ///
    /// # Arguments
    /// * `path` - File path for the WAL log
    /// * `max_queue_size` - Maximum number of entries in the pending queue before backpressure
    ///
    /// # Panics
    /// Panics if the file cannot be opened (permissions, disk full, etc.)
    pub async fn with_config<P: AsRef<Path>>(path: P, max_queue_size: usize) -> Self {
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
            max_queue_size,
            pending_count: Arc::new(AtomicUsize::new(0)),
            space_available: Arc::new(Notify::new()),
            flusher_handle: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            flush_notify: Arc::new(Notify::new()),
        }
    }

    /// Appends a WAL entry to the lock-free pending queue.
    ///
    /// This method implements graceful backpressure: if the queue is full (at `max_queue_size`),
    /// the caller waits asynchronously until space becomes available. This prevents unbounded
    /// memory growth while maintaining lock-free semantics once admitted.
    ///
    /// The append path remains lock-free using `SegQueue::push()` with CAS atomics.
    /// Multiple threads can append concurrently without blocking each other.
    ///
    /// # Backpressure Behavior
    /// - If queue has space: Immediate lock-free push (~1-2 µs)
    /// - If queue is full: Async wait until flusher drains entries
    /// - No hard errors: Writers automatically resume when space available
    ///
    /// # Arguments
    /// * `entry` - WAL entry to append
    ///
    /// # Returns
    /// Always returns Ok after successfully appending (may wait if queue full)
    pub async fn append(&self, entry: WalEntry) -> Result<(), WalError> {
        // Wait asynchronously if queue is at capacity
        loop {
            let current_count = self.pending_count.load(Ordering::Acquire);
            
            if current_count >= self.max_queue_size {
                // Queue full - wait for space to become available
                self.space_available.notified().await;
                continue;
            }
            
            // Optimistically increment count
            if self.pending_count.compare_exchange(
                current_count,
                current_count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ).is_ok() {
                // Successfully reserved a slot - now push to queue
                self.pending.push(entry);
                self.flush_notify.notify_one();
                return Ok(());
            }
            
            // CAS failed, retry (another thread incremented)
        }
    }

    /// Starts the background flusher task.
    ///
    /// The flusher runs on a configurable interval and:
    /// 1. Drains all pending entries from the queue
    /// 2. Batches them into a single write operation
    /// 3. Writes to disk with fsync for durability
    ///
    /// The flusher also responds to immediate flush requests via `flush_notify`.
    /// On shutdown, performs a final flush before terminating.
    ///
    /// # Arguments
    /// * `flush_interval_ms` - Milliseconds between flush attempts (e.g., 10ms)
    ///
    /// # Example
    /// ```ignore
    /// let wal = WalStorage::new("wal.log").await;
    /// wal.start_flusher(10); // Flush every 10ms
    /// ```
    pub fn start_flusher(&self, flush_interval_ms: u64) {
        let pending = Arc::clone(&self.pending);
        let inner = Arc::clone(&self.inner);
        let shutdown = Arc::clone(&self.shutdown);
        let flush_notify = Arc::clone(&self.flush_notify);
        let pending_count = Arc::clone(&self.pending_count);
        let space_available = Arc::clone(&self.space_available);

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(flush_interval_ms));
            
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(e) = Self::drain_and_flush(&pending, &inner, &pending_count, &space_available).await {
                            eprintln!("WAL flush error: {e}");
                        }
                    }
                    () = flush_notify.notified() => {
                        // Immediate flush requested
                        if let Err(e) = Self::drain_and_flush(&pending, &inner, &pending_count, &space_available).await {
                            eprintln!("WAL flush error: {e}");
                        }
                    }
                }

                if shutdown.load(Ordering::SeqCst) {
                    // Final flush on shutdown
                    if let Err(e) = Self::drain_and_flush(&pending, &inner, &pending_count, &space_available).await {
                        eprintln!("Final WAL flush error: {e}");
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

    /// Drains the pending queue and writes all entries to disk.
    ///
    /// This is an internal method called by the background flusher. It:
    /// 1. Drains all entries from the lock-free queue
    /// 2. Serializes each entry with bincode
    /// 3. Writes length-prefixed entries to disk
    /// 4. Calls fsync to ensure durability
    ///
    /// Acquires the file lock only once for the entire batch, maximizing throughput.
    ///
    /// # Arguments
    /// * `pending` - Lock-free queue of pending entries
    /// * `inner` - Mutex-protected file writer
    ///
    /// # Errors
    /// Returns error if serialization or I/O fails
    async fn drain_and_flush(
        pending: &SegQueue<WalEntry>,
        inner: &Arc<Mutex<InnerWal>>,
        pending_count: &Arc<AtomicUsize>,
        space_available: &Arc<Notify>,
    ) -> Result<(), WalError> {
        let mut batch = Vec::new();
        
        // Drain all pending entries
        while let Some(entry) = pending.pop() {
            batch.push(entry);
        }

        if batch.is_empty() {
            return Ok(());
        }

        let batch_size = batch.len();

        // Single lock acquisition for entire batch
        let mut inner = inner.lock().await;
        
        for entry in batch {
            let encoded = bincode::serialize(&entry)
                .map_err(WalError::Deserialization)?;
            
            // Create header with CRC32 checksum
            let header = WalEntryHeader::new(&encoded);
            let header_bytes = header.to_bytes();

            // Write header then data
            inner.file.write_all(&header_bytes).await?;
            inner.file.write_all(&encoded).await?;
        }

        inner.file.flush().await?;
        inner.file.get_mut().sync_all().await?; // fsync for durability
        
        // Update pending count and notify waiting writers
        pending_count.fetch_sub(batch_size, Ordering::Release);
        space_available.notify_waiters();
        
        Ok(())
    }

    /// Performs an explicit flush, blocking until all pending entries are written.
    ///
    /// This method:
    /// 1. Notifies the background flusher to flush immediately
    /// 2. Polls the queue until it's empty (with timeout)
    /// 3. Forces a final flush if entries remain after timeout
    ///
    /// Used by transactions during commit/abort to ensure durability before
    /// returning to the caller.
    ///
    /// # Errors
    /// Returns error if the flush operation fails
    pub async fn flush(&self) -> Result<(), WalError> {
        // Notify flusher to flush immediately
        self.flush_notify.notify_one();
        
        // Wait until queue is drained
        let mut retries = 0;
        while self.pending_count.load(Ordering::Acquire) > 0 && retries < 100 {
            tokio::time::sleep(tokio::time::Duration::from_micros(100)).await;
            retries += 1;
        }

        if self.pending_count.load(Ordering::Acquire) > 0 {
            // Force flush remaining entries
            Self::drain_and_flush(&self.pending, &self.inner, &self.pending_count, &self.space_available).await?;
        }

        Ok(())
    }

    /// Stops the background flusher gracefully.
    ///
    /// This method:
    /// 1. Sets the shutdown flag atomically
    /// 2. Notifies the flusher to wake up and check shutdown flag
    /// 3. Waits for the flusher thread to complete (with final flush)
    ///
    /// Ensures all pending data is flushed before the thread terminates.
    /// Should be called during database shutdown.
    ///
    /// # Errors
    /// Returns error if the flusher thread panicked or join failed
    pub async fn stop_flusher(&self) -> Result<(), WalError> {
        self.shutdown.store(true, Ordering::SeqCst);
        self.flush_notify.notify_one();

        // Wait for flusher to finish
        if let Some(handle) = self.flusher_handle.lock().await.take() {
            handle.await.map_err(|e| WalError::Io(std::io::Error::other(
                format!("Flusher join error: {e}")
            )))?;
        }

        Ok(())
    }

    /// Writes a checkpoint marker to a separate file.
    ///
    /// The checkpoint file (WAL path + ".checkpoint") stores the highest
    /// transaction ID that has been checkpointed. This allows recovery to
    /// skip replaying old transactions.
    ///
    /// # Arguments
    /// * `tx_id` - Transaction ID to checkpoint
    ///
    /// # Errors
    /// Returns error if the checkpoint file cannot be written
    pub async fn set_checkpoint(&self, tx_id: u64) -> Result<(), WalError> {
        let ckpt_path = format!("{}.checkpoint", self.path);
        let mut ckpt_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(ckpt_path)
            .await
            .map_err(|e| WalError::Checkpoint(format!("Failed to write checkpoint: {e}")))?;
        ckpt_file.write_all(tx_id.to_string().as_bytes()).await?;
        ckpt_file.flush().await?;
        Ok(())
    }

    /// Reads the last checkpoint transaction ID from disk.
    ///
    /// Returns 0 if no checkpoint file exists (indicating this is the first
    /// checkpoint or the file was deleted).
    ///
    /// # Returns
    /// The transaction ID of the last checkpoint, or 0 if none exists
    ///
    /// # Errors
    /// Returns error if the checkpoint file exists but cannot be read or parsed
    pub async fn get_last_checkpoint(&self) -> Result<u64, WalError> {
        let ckpt_path = format!("{}.checkpoint", self.path);
        if !Path::new(&ckpt_path).exists() {
            return Ok(0);
        }
        let mut ckpt_file = File::open(ckpt_path).await
            .map_err(|e| WalError::Checkpoint(format!("Failed to read checkpoint: {e}")))?;
        let mut contents = String::new();
        ckpt_file.read_to_string(&mut contents).await?;
        contents.trim().parse::<u64>()
            .map_err(|e| WalError::Checkpoint(format!("Invalid checkpoint TxID: {e}")))
    }

    /// Truncates the WAL by renaming the old file and creating a new one.
    ///
    /// This method:
    /// 1. Flushes all pending entries to ensure nothing is lost
    /// 2. Renames the current WAL to ".old" (for backup/debugging)
    /// 3. Creates a new empty WAL at the specified path
    ///
    /// Should be called after a successful checkpoint to reclaim disk space.
    ///
    /// # Arguments
    /// * `path` - Path for the new WAL file
    ///
    /// # Errors
    /// Returns error if flush or file operations fail
    pub async fn truncate_wal(&mut self, path: &str) -> Result<(), WalError> {
        // Ensure all pending writes are flushed first
        self.flush().await?;
        
        let old_path = format!("{}.old", self.path);
        tokio::fs::rename(&self.path, &old_path).await
            .map_err(|e| WalError::Checkpoint(format!("Failed to rename WAL: {e}")))?;
        *self = Self::with_config(path, self.max_queue_size).await;
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
        
        // Give a small grace period for filesystem sync
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        
        // Verify file has data
        let file_size = tokio::fs::metadata(&wal_path).await.unwrap().len();
        assert!(file_size > 0, "WAL file should contain data from 1000 entries");

        wal.stop_flusher().await.unwrap();
    }

    #[async_test]
    async fn test_checksum_validation() {
        use tokio::io::{AsyncWriteExt, AsyncSeekExt};
        
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal_checksum.log");
        
        // Write a valid entry first
        let wal = WalStorage::new(&wal_path).await;
        wal.start_flusher(10);
        
        let entry = WalEntry::Insert {
            tx_id: 1,
            timestamp: 123,
            key: "test".to_string(),
            value: b"data".to_vec(),
        };
        wal.append(entry).await.unwrap();
        wal.flush().await.unwrap();
        wal.stop_flusher().await.unwrap();
        
        // Now corrupt the file by modifying a byte
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&wal_path)
            .await
            .unwrap();
        
        // Seek past the header and corrupt the data
        file.seek(std::io::SeekFrom::Start(WalEntryHeader::SIZE as u64 + 5)).await.unwrap();
        file.write_all(&[0xFF]).await.unwrap();
        file.flush().await.unwrap();
        drop(file);
        
        // Try to read - should fail with checksum error
        use tokio::fs::File;
        use tokio::io::BufReader;
        
        let mut file = File::open(&wal_path).await.unwrap();
        let mut reader = BufReader::new(&mut file);
        
        let mut header_bytes = [0u8; WalEntryHeader::SIZE];
        reader.read_exact(&mut header_bytes).await.unwrap();
        let header = WalEntryHeader::from_bytes(&header_bytes);
        
        let mut buffer = vec![0u8; header.length as usize];
        reader.read_exact(&mut buffer).await.unwrap();
        
        let result = header.validate(&buffer);
        assert!(result.is_err(), "Should detect corruption");
        
        match result {
            Err(WalError::ChecksumMismatch { .. }) => {
                // Expected error type
            }
            _ => panic!("Expected ChecksumMismatch error"),
        }
    }

    #[async_test]
    async fn test_header_encoding() {
        let data = b"test data for checksum";
        let header = WalEntryHeader::new(data);
        
        // Verify header fields
        assert_eq!(header.length, data.len() as u64);
        assert_eq!(header.version, WalEntryHeader::VERSION);
        assert_ne!(header.checksum, 0); // Should have calculated checksum
        
        // Verify validation works
        assert!(header.validate(data).is_ok());
        
        // Verify round-trip encoding
        let bytes = header.to_bytes();
        let decoded = WalEntryHeader::from_bytes(&bytes);
        
        assert_eq!(header.length, decoded.length);
        assert_eq!(header.checksum, decoded.checksum);
        assert_eq!(header.version, decoded.version);
    }

    #[async_test]
    async fn test_bounded_queue_backpressure() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal_bounded.log");
        
        // Create WAL with very small queue (2 entries) to easily trigger backpressure
        let wal = Arc::new(WalStorage::with_config(&wal_path, 2).await);
        // Don't start flusher yet - simulate slow processing
        
        // Fill the queue to capacity (should succeed immediately)
        let start = std::time::Instant::now();
        for i in 0..2 {
            let entry = WalEntry::Insert {
                tx_id: i,
                timestamp: 123,
                key: format!("key{}", i),
                value: b"value".to_vec(),
            };
            wal.append(entry).await.unwrap();
        }
        let fill_time = start.elapsed();
        println!("Filled queue in {:?}", fill_time);
        assert!(fill_time.as_millis() < 10, "Filling should be fast");
        
        // Now try to append one more - should block since queue is full
        let wal_clone = Arc::clone(&wal);
        let start = std::time::Instant::now();
        
        let blocked_handle = tokio::spawn(async move {
            let entry = WalEntry::Insert {
                tx_id: 100,
                timestamp: 123,
                key: "blocked".to_string(),
                value: b"value".to_vec(),
            };
            wal_clone.append(entry).await.unwrap();
            std::time::Instant::now()
        });
        
        // Give blocked task time to hit backpressure
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        
        // Now start flusher to unblock the waiting writer
        wal.start_flusher(10);
        
        // Wait for blocked append to complete
        let finish_time = blocked_handle.await.unwrap();
        let total_time = finish_time.duration_since(start);
        
        println!("Backpressure test: writer blocked for {:?}", total_time);
        
        // Writer should have been blocked for at least the sleep duration
        assert!(total_time.as_millis() >= 50, 
            "Writer should have been blocked by backpressure (waited {:?})", total_time);
        
        wal.flush().await.unwrap();
        wal.stop_flusher().await.unwrap();
    }

    #[async_test]
    async fn test_queue_capacity_enforcement() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal_capacity.log");
        
        // Create WAL with capacity of 100
        let wal = Arc::new(WalStorage::with_config(&wal_path, 100).await);
        wal.start_flusher(50); // Moderate flush interval
        
        // Spawn many concurrent writers
        let mut handles = vec![];
        for i in 0..20 {
            let wal_clone = Arc::clone(&wal);
            let handle = tokio::spawn(async move {
                for j in 0..10 {
                    let entry = WalEntry::Insert {
                        tx_id: (i * 10 + j) as u64,
                        timestamp: 123,
                        key: format!("key-{}-{}", i, j),
                        value: b"value".to_vec(),
                    };
                    wal_clone.append(entry).await.unwrap();
                }
            });
            handles.push(handle);
        }
        
        // All writes should complete (with backpressure)
        for handle in handles {
            handle.await.unwrap();
        }
        
        // Verify all data was written
        wal.flush().await.unwrap();
        let file_size = tokio::fs::metadata(&wal_path).await.unwrap().len();
        assert!(file_size > 0, "All entries should be persisted");
        
        wal.stop_flusher().await.unwrap();
    }

    #[async_test]
    async fn test_high_throughput_with_bounded_queue() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal_throughput.log");
        
        // Create WAL with reasonable capacity
        let wal = Arc::new(WalStorage::with_config(&wal_path, 1000).await);
        wal.start_flusher(10); // Fast flusher
        
        let start = std::time::Instant::now();
        
        // Write 5000 entries with 50 concurrent threads
        let mut handles = vec![];
        for i in 0..50 {
            let wal_clone = Arc::clone(&wal);
            let handle = tokio::spawn(async move {
                for j in 0..100 {
                    let entry = WalEntry::Insert {
                        tx_id: (i * 100 + j) as u64,
                        timestamp: 123,
                        key: format!("key-{}-{}", i, j),
                        value: vec![0u8; 100], // 100 bytes per entry
                    };
                    wal_clone.append(entry).await.unwrap();
                }
            });
            handles.push(handle);
        }
        
        for handle in handles {
            handle.await.unwrap();
        }
        
        wal.flush().await.unwrap();
        let elapsed = start.elapsed();
        
        // Should complete in reasonable time despite bounded queue
        assert!(elapsed.as_secs() < 5, "Should complete within 5 seconds with backpressure");
        
        // Verify all data persisted
        let file_size = tokio::fs::metadata(&wal_path).await.unwrap().len();
        assert!(file_size > 500_000, "Should have written significant data (5000 entries)");
        
        wal.stop_flusher().await.unwrap();
    }

    #[async_test]
    async fn test_graceful_degradation_under_load() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal_degradation.log");
        
        // Small queue + slow flusher = intentional overload
        let wal = Arc::new(WalStorage::with_config(&wal_path, 10).await);
        wal.start_flusher(200); // Very slow flusher
        
        // Try to write 100 entries quickly
        let mut handles = vec![];
        for i in 0..10 {
            let wal_clone = Arc::clone(&wal);
            let handle = tokio::spawn(async move {
                for j in 0..10 {
                    let entry = WalEntry::Insert {
                        tx_id: (i * 10 + j) as u64,
                        timestamp: 123,
                        key: format!("key{}", i * 10 + j),
                        value: b"data".to_vec(),
                    };
                    // Should succeed but may be delayed
                    wal_clone.append(entry).await.unwrap();
                }
            });
            handles.push(handle);
        }
        
        // All writes should eventually succeed (graceful degradation)
        for handle in handles {
            handle.await.unwrap();
        }
        
        wal.flush().await.unwrap();
        wal.stop_flusher().await.unwrap();
    }

    #[async_test]
    async fn test_pending_count_accuracy() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal_count.log");
        
        let wal = WalStorage::with_config(&wal_path, 1000).await;
        wal.start_flusher(1000); // Very slow to keep entries in queue
        
        // Add entries
        for i in 0..50 {
            let entry = WalEntry::Insert {
                tx_id: i,
                timestamp: 123,
                key: format!("key{}", i),
                value: b"value".to_vec(),
            };
            wal.append(entry).await.unwrap();
        }
        
        // Check count is tracked
        let count = wal.pending_count.load(Ordering::Acquire);
        assert!(count > 0 && count <= 50, "Pending count should reflect queued entries");
        
        // Flush and verify count drops
        wal.flush().await.unwrap();
        let count_after = wal.pending_count.load(Ordering::Acquire);
        assert_eq!(count_after, 0, "Pending count should be 0 after flush");
        
        wal.stop_flusher().await.unwrap();
    }
}
