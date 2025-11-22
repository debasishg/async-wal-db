use crate::{Transaction, WalEntry, WalError, WalStorage, WalEntryHeader, NEXT_TX_ID};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::RwLock;

/// Main database structure with lock-free WAL and in-memory `HashMap` storage.
///
/// The database consists of:
/// - `store`: In-memory `HashMap` with `RwLock` for concurrent reads
/// - `wal`: Lock-free Write-Ahead Log for durability
///
/// All writes go through the WAL first, then are applied to the store on commit.
pub struct Database {
    pub store: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    pub wal: WalStorage,
}

impl Database {
    /// Creates a new database instance with the specified WAL path.
    ///
    /// This method:
    /// 1. Initializes the WAL storage at the given path
    /// 2. Starts the background flusher with a 10ms interval
    /// 3. Creates an empty in-memory `HashMap` store
    ///
    /// The database is ready to accept transactions immediately after creation.
    /// To restore from existing WAL, call `recover()` after `new()`.
    ///
    /// # Arguments
    /// * `wal_path` - File path for the WAL log
    ///
    /// # Returns
    /// Arc-wrapped Database instance for shared ownership across threads
    pub async fn new(wal_path: &str) -> Arc<Self> {
        let wal = WalStorage::new(wal_path).await;
        // Start background flusher with 10ms interval
        wal.start_flusher(10);
        Arc::new(Self {
            store: Arc::new(RwLock::new(HashMap::new())),
            wal,
        })
    }

    /// Begins a new transaction with a unique ID.
    ///
    /// Transactions are initially in Active state and can perform multiple
    /// operations before committing or aborting.
    ///
    /// # Returns
    /// A new Transaction instance ready to accept operations
    pub fn begin_transaction(&self) -> Transaction {
        Transaction::new()
    }

    /// Recovers the database state by replaying the WAL from disk.
    ///
    /// This method:
    /// 1. Reads all entries from the WAL file
    /// 2. Groups entries by transaction ID
    /// 3. Applies only committed transactions to the store (in order)
    /// 4. Restores the transaction ID counter to prevent ID reuse
    ///
    /// Should be called once after database creation to restore state from
    /// a previous run. Safe to call on an empty WAL (does nothing).
    ///
    /// # Errors
    /// Returns error if WAL cannot be opened, entries cannot be deserialized,
    /// or validation fails (e.g., empty keys).
    pub async fn recover(&self) -> Result<(), WalError> {
        let mut file = File::open(&self.wal.path).await
            .map_err(|_| WalError::OpenWal)?;
        let mut reader = BufReader::new(&mut file);

        let mut tx_entries: HashMap<u64, Vec<WalEntry>> = HashMap::new();
        let mut committed_txs: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut buffer = Vec::new();

        loop {
            // Read header
            let mut header_bytes = [0u8; WalEntryHeader::SIZE];
            match reader.read_exact(&mut header_bytes).await {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(WalError::from(e)),
            }
            
            let header = WalEntryHeader::from_bytes(&header_bytes);
            
            // Read entry data
            #[allow(clippy::cast_possible_truncation)]
            let len = header.length as usize;
            buffer.resize(len, 0);
            reader.read_exact(&mut buffer).await?;
            
            // Validate checksum
            header.validate(&buffer)?;

            let entry = bincode::deserialize::<WalEntry>(&buffer)
                .map_err(WalError::Deserialization)?;

            let tx_id = match &entry {
                WalEntry::Commit { tx_id, .. }
                | WalEntry::Abort { tx_id, .. }
                | WalEntry::Insert { tx_id, .. }
                | WalEntry::Update { tx_id, .. }
                | WalEntry::Delete { tx_id, .. } => *tx_id,
            };
            tx_entries.entry(tx_id).or_default().push(entry.clone());

            match entry {
                WalEntry::Commit { tx_id, .. } => { committed_txs.insert(tx_id); }
                WalEntry::Abort { tx_id, .. } => { committed_txs.remove(&tx_id); }
                _ => {}
            }
        }

        let mut store = self.store.write().await;
        let mut sorted_tx_ids: Vec<u64> = committed_txs.iter().copied().collect();
        sorted_tx_ids.sort_unstable();

        for tx_id in sorted_tx_ids {
            if let Some(entries) = tx_entries.get(&tx_id) {
                for entry in entries.iter().filter(|e| !matches!(e, WalEntry::Commit { .. } | WalEntry::Abort { .. })) {
                    match entry {
                        WalEntry::Insert { key, value, .. } => { store.insert(key.clone(), value.clone()); }
                        WalEntry::Update { key, new_value, .. } => { store.insert(key.clone(), new_value.clone()); }
                        WalEntry::Delete { key, .. } => { store.remove(key); }
                        _ => {}
                    }
                }
            }
        }

        for key in (*store).keys() {
            if key.is_empty() {
                return Err(WalError::Validation("Invalid empty key after recovery".to_string()));
            }
        }

        // Restore transaction ID counter to avoid ID reuse after restart
        if let Some(&max_tx_id) = tx_entries.keys().max() {
            NEXT_TX_ID.fetch_max(max_tx_id + 1, Ordering::SeqCst);
        }

        println!("Recovery complete: {} committed txns replayed", committed_txs.len());
        Ok(())
    }

    /// Creates a checkpoint by compacting the WAL and advancing the checkpoint marker.
    ///
    /// This method:
    /// 1. Reads all WAL entries since the last checkpoint
    /// 2. Applies committed transactions to the store
    /// 3. Updates the checkpoint marker to the highest committed transaction ID
    ///
    /// Checkpoints allow for faster recovery by avoiding replay of old transactions.
    /// The WAL can be truncated after a checkpoint (future enhancement).
    ///
    /// # Returns
    /// The transaction ID of the new checkpoint
    ///
    /// # Errors
    /// Returns `NoNewCheckpoints` if there are no new committed transactions since
    /// the last checkpoint. Returns other errors if WAL cannot be read or entries
    /// cannot be deserialized.
    pub async fn checkpoint(&self) -> Result<u64, WalError> {
        let last_ckpt = self.wal.get_last_checkpoint().await?;
        if last_ckpt == 0 && self.wal.path.is_empty() {
            return Err(WalError::NoNewCheckpoints);
        }

        let mut file = File::open(&self.wal.path).await
            .map_err(|_| WalError::OpenWal)?;
        let mut reader = BufReader::new(&mut file);
        let mut tx_entries: HashMap<u64, Vec<WalEntry>> = HashMap::new();
        let mut committed_txs: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut buffer = Vec::new();

        loop {
            // Read header
            let mut header_bytes = [0u8; WalEntryHeader::SIZE];
            match reader.read_exact(&mut header_bytes).await {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(WalError::from(e)),
            }
            
            let header = WalEntryHeader::from_bytes(&header_bytes);
            
            // Read entry data
            #[allow(clippy::cast_possible_truncation)]
            let len = header.length as usize;
            buffer.resize(len, 0);
            reader.read_exact(&mut buffer).await?;
            
            // Validate checksum
            header.validate(&buffer)?;

            let entry = bincode::deserialize::<WalEntry>(&buffer)
                .map_err(WalError::Deserialization)?;

            let tx_id = match &entry {
                WalEntry::Commit { tx_id, .. }
                | WalEntry::Abort { tx_id, .. }
                | WalEntry::Insert { tx_id, .. }
                | WalEntry::Update { tx_id, .. }
                | WalEntry::Delete { tx_id, .. } => *tx_id,
            };

            if tx_id <= last_ckpt { continue; }

            tx_entries.entry(tx_id).or_default().push(entry.clone());

            match entry {
                WalEntry::Commit { tx_id, .. } => { committed_txs.insert(tx_id); }
                WalEntry::Abort { tx_id, .. } => { committed_txs.remove(&tx_id); }
                _ => {}
            }
        }

        if committed_txs.is_empty() {
            return Err(WalError::NoNewCheckpoints);
        }

        let mut store = self.store.write().await;
        let mut sorted_tx_ids: Vec<u64> = committed_txs.iter().copied().filter(|&id| id > last_ckpt).collect();
        sorted_tx_ids.sort_unstable();

        for tx_id in sorted_tx_ids {
            if let Some(entries) = tx_entries.get(&tx_id) {
                for entry in entries.iter().filter(|e| !matches!(e, WalEntry::Commit { .. } | WalEntry::Abort { .. })) {
                    match entry {
                        WalEntry::Insert { key, value, .. } => { store.insert(key.clone(), value.clone()); }
                        WalEntry::Update { key, new_value, .. } => { store.insert(key.clone(), new_value.clone()); }
                        WalEntry::Delete { key, .. } => { store.remove(key); }
                        _ => {}
                    }
                }
            }
        }

        let max_ckpt = *committed_txs.iter().max().unwrap_or(&last_ckpt);
        self.wal.set_checkpoint(max_ckpt).await?;

        println!("Checkpoint complete: Advanced to TxID {max_ckpt}");
        Ok(max_ckpt)
    }

    /// Starts a background task that periodically creates checkpoints.
    ///
    /// The scheduler runs on the specified interval and attempts to checkpoint
    /// the database. It gracefully handles the case where no new transactions
    /// exist (common scenario).
    ///
    /// # Arguments
    /// * `interval_secs` - Seconds between checkpoint attempts
    ///
    /// # Returns
    /// `JoinHandle` for the background task (can be used to cancel/await)
    ///
    /// # Example
    /// ```ignore
    /// let db = Database::new("wal.log").await;
    /// let scheduler = db.start_checkpoint_scheduler(60); // Every minute
    /// ```
    pub fn start_checkpoint_scheduler(self: Arc<Self>, interval_secs: u64) -> tokio::task::JoinHandle<()> {
        let db_clone = Arc::clone(&self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                match db_clone.checkpoint().await {
                    Ok(tx_id) => println!("Auto-checkpoint completed at TxID: {tx_id}"),
                    Err(WalError::NoNewCheckpoints) => {
                        // This is normal - no new transactions to checkpoint
                    }
                    Err(e) => eprintln!("Checkpoint failed: {e}"),
                }
            }
        })
    }

    /// Performs a graceful shutdown of the database.
    ///
    /// This method:
    /// 1. Signals the background flusher to stop
    /// 2. Performs a final flush of all pending WAL entries
    /// 3. Waits for the flusher thread to terminate
    ///
    /// Should be called before dropping the database to ensure all data is
    /// persisted to disk. After shutdown, the database should not be used.
    ///
    /// # Errors
    /// Returns error if the final flush fails or the flusher thread panicked.
    pub async fn shutdown(&self) -> Result<(), WalError> {
        self.wal.stop_flusher().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::test as async_test;

    #[async_test]
    async fn test_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log").to_str().unwrap().to_string();
        let db = Database::new(&wal_path).await;
        // Simulate writes
        let mut tx = db.begin_transaction();
        tx.append_op(&db, WalEntry::Insert {
            tx_id: 0, timestamp: 0, key: "recovered".to_string(), value: b"data".to_vec()
        }).await.unwrap();
        tx.commit(&db).await.unwrap();
        drop(db);  // Close

        let db_new = Database::new(&wal_path).await;
        db_new.recover().await.unwrap();
        let store = db_new.store.read().await;
        assert_eq!(store.get("recovered").map(|v| v.as_slice()), Some(&b"data"[..]));
    }

    #[async_test]
    async fn test_checkpoint_compaction() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log").to_str().unwrap().to_string();
        let db = Database::new(&wal_path).await;
        // Commit a txn
        let mut tx = db.begin_transaction();
        tx.append_op(&db, WalEntry::Insert {
            tx_id: 0, timestamp: 0, key: "ckpt".to_string(), value: b"data".to_vec()
        }).await.unwrap();
        tx.commit(&db).await.unwrap();

        db.checkpoint().await.unwrap();
        // Checkpoint updates the checkpoint file but doesn't truncate WAL
        let wal_size = tokio::fs::metadata(&wal_path).await.unwrap().len();
        assert!(wal_size > 0);  // WAL still contains data
        let store = db.store.read().await;
        assert_eq!(store.get("ckpt").map(|v| v.as_slice()), Some(&b"data"[..]));
    }

    #[async_test]
    async fn test_transaction_id_uniqueness_after_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal_unique.log").to_str().unwrap().to_string();
        
        // Create first transaction
        let db = Database::new(&wal_path).await;
        let tx1 = db.begin_transaction();
        let tx1_id = tx1.id;
        drop(tx1);
        drop(db);

        // Recover and create new transaction - should have higher ID
        let db2 = Database::new(&wal_path).await;
        db2.recover().await.unwrap();
        let tx2 = db2.begin_transaction();
        assert!(tx2.id > tx1_id, "Transaction ID after recovery ({}) should be greater than before ({})", tx2.id, tx1_id);
    }
}
