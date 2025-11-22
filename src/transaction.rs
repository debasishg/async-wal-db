use crate::{Database, WalEntry, WalError, NEXT_TX_ID};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Debug)]
pub struct Transaction {
    pub id: u64,
    pub state: TxState,
    pub entries: Vec<WalEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxState { Active, Committed, Aborted }

impl Default for Transaction {
    fn default() -> Self {
        Self::new()
    }
}

impl Transaction {
    /// Creates a new transaction with a unique ID.
    ///
    /// The transaction ID is atomically incremented from a global counter,
    /// ensuring uniqueness across all transactions (even after recovery).
    /// The initial state is `TxState::Active`.
    pub fn new() -> Self {
        Self {
            id: NEXT_TX_ID.fetch_add(1, Ordering::SeqCst),
            state: TxState::Active,
            entries: Vec::new(),
        }
    }

    /// Adds a WAL entry to the transaction's local buffer.
    ///
    /// This does not write to the WAL immediately - entries are buffered
    /// in memory until commit or abort is called.
    pub fn add_entry(&mut self, entry: WalEntry) {
        self.entries.push(entry);
    }

    /// Appends an operation to both the transaction buffer and the WAL.
    ///
    /// This method:
    /// 1. Sets the transaction ID and timestamp on the operation
    /// 2. Adds the operation to the local transaction buffer
    /// 3. Immediately appends to the WAL (lock-free via `SegQueue`)
    ///
    /// The operation is not applied to the store until `commit()` is called.
    ///
    /// # Arguments
    /// * `db` - Database reference for WAL access
    /// * `op` - Operation to append (Insert, Update, or Delete)
    ///
    /// # Errors
    /// Returns error if the operation is not Insert/Update/Delete or if WAL append fails.
    pub async fn append_op(&mut self, db: &Arc<Database>, mut op: WalEntry) -> Result<(), WalError> {
        match &mut op {
            WalEntry::Insert { tx_id, timestamp, .. }
            | WalEntry::Update { tx_id, timestamp, .. }
            | WalEntry::Delete { tx_id, timestamp, .. } => {
                *tx_id = self.id;
                *timestamp = WalEntry::new_timestamp();
            }
            _ => return Err(WalError::InvalidState("Cannot append non-operation entry".to_string())),
        }
        self.add_entry(op.clone());
        db.wal.append(op).await
    }

    /// Applies a single operation to the in-memory store.
    ///
    /// This method acquires a write lock on the `HashMap` and applies the operation:
    /// - Insert: Adds key-value pair
    /// - Update: Modifies existing value (fails if key doesn't exist)
    /// - Delete: Removes key-value pair
    ///
    /// # Arguments
    /// * `db` - Database reference for store access
    /// * `op` - WAL entry to apply (must be Insert/Update/Delete)
    ///
    /// # Errors
    /// Returns error if trying to update a non-existent key or if entry type is invalid.
    pub async fn apply_to_store(&self, db: &Arc<Database>, op: &WalEntry) -> Result<(), WalError> {
        let mut store = db.store.write().await;
        match op {
            WalEntry::Insert { key, value, .. } => {
                store.insert(key.clone(), value.clone());
            }
            WalEntry::Update { key, new_value, .. } => {
                if let Some(v) = store.get_mut(key) {
                    v.clone_from(new_value);
                } else {
                    return Err(WalError::ApplyStore(format!("Update on non-existent key: {key}")));
                }
            }
            WalEntry::Delete { key, .. } => {
                store.remove(key);
            }
            _ => return Err(WalError::InvalidState("Cannot apply non-operation entry to store".to_string())),
        }
        Ok(())
    }

    /// Commits the transaction, making all changes durable and visible.
    ///
    /// This method performs the following steps:
    /// 1. Writes a Commit entry to the WAL
    /// 2. Flushes the WAL to disk (fsync for durability)
    /// 3. Applies all buffered operations to the in-memory store
    /// 4. Marks transaction as Committed
    ///
    /// After commit, changes are durable (survive crashes) and visible to other transactions.
    ///
    /// # Arguments
    /// * `db` - Database reference
    ///
    /// # Errors
    /// Returns error if transaction is not Active or if any operation fails.
    pub async fn commit(mut self, db: &Arc<Database>) -> Result<(), WalError> {
        if self.state != TxState::Active {
            return Err(WalError::InvalidState("Cannot commit non-active transaction".to_string()));
        }

        let commit_entry = WalEntry::Commit { 
            tx_id: self.id, 
            timestamp: WalEntry::new_timestamp() 
        };
        db.wal.append(commit_entry).await?;
        db.wal.flush().await?;

        self.state = TxState::Committed;
        for entry in &self.entries {
            self.apply_to_store(db, entry).await?;
        }
        Ok(())
    }

    /// Aborts the transaction, discarding all changes.
    ///
    /// This method:
    /// 1. Writes an Abort entry to the WAL
    /// 2. Flushes the WAL to disk
    /// 3. Clears all buffered operations (changes are not applied to store)
    /// 4. Marks transaction as Aborted
    ///
    /// After abort, no changes are visible and the transaction is rolled back.
    ///
    /// # Arguments
    /// * `db` - Database reference
    ///
    /// # Errors
    /// Returns error if transaction is not Active or if WAL operations fail.
    pub async fn abort(mut self, db: &Arc<Database>) -> Result<(), WalError> {
        if self.state != TxState::Active {
            return Err(WalError::InvalidState("Cannot abort non-active transaction".to_string()));
        }

        let abort_entry = WalEntry::Abort { 
            tx_id: self.id, 
            timestamp: WalEntry::new_timestamp() 
        };
        db.wal.append(abort_entry).await?;
        db.wal.flush().await?;
        self.state = TxState::Aborted;
        self.entries.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::test as async_test;

    #[async_test]
    async fn test_transaction_commit() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log").to_str().unwrap().to_string();
        let db = Database::new(&wal_path).await;
        let mut tx = db.begin_transaction();
        tx.append_op(&db, WalEntry::Insert {
            tx_id: 0, timestamp: 0, key: "key".to_string(), value: b"value".to_vec()
        }).await.unwrap();
        tx.commit(&db).await.unwrap();

        let store = db.store.read().await;
        assert_eq!(store.get("key").map(|v| v.as_slice()), Some(&b"value"[..]));
    }

    #[async_test]
    async fn test_transaction_abort() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log").to_str().unwrap().to_string();
        let db = Database::new(&wal_path).await;
        let mut tx = db.begin_transaction();
        tx.append_op(&db, WalEntry::Insert {
            tx_id: 0, timestamp: 0, key: "key".to_string(), value: b"value".to_vec()
        }).await.unwrap();
        tx.abort(&db).await.unwrap();

        let store = db.store.read().await;
        assert_eq!(store.get("key"), None);
    }
}
