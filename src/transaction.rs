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

impl Transaction {
    pub fn new() -> Self {
        Self {
            id: NEXT_TX_ID.fetch_add(1, Ordering::SeqCst),
            state: TxState::Active,
            entries: Vec::new(),
        }
    }

    pub fn add_entry(&mut self, entry: WalEntry) {
        self.entries.push(entry);
    }

    pub async fn append_op(&mut self, db: &Arc<Database>, mut op: WalEntry) -> Result<(), WalError> {
        match &mut op {
            WalEntry::Insert { tx_id, timestamp, .. } => {
                *tx_id = self.id;
                *timestamp = WalEntry::new_timestamp();
            }
            WalEntry::Update { tx_id, timestamp, .. } => {
                *tx_id = self.id;
                *timestamp = WalEntry::new_timestamp();
            }
            WalEntry::Delete { tx_id, timestamp, .. } => {
                *tx_id = self.id;
                *timestamp = WalEntry::new_timestamp();
            }
            _ => return Err(WalError::InvalidState("Cannot append non-operation entry".to_string())),
        }
        self.add_entry(op.clone());
        db.wal.append(&op).await
    }

    pub async fn apply_to_store(&self, db: &Arc<Database>, op: &WalEntry) -> Result<(), WalError> {
        let mut store = db.store.write().await;
        match op {
            WalEntry::Insert { key, value, .. } => {
                store.insert(key.clone(), value.clone());
            }
            WalEntry::Update { key, new_value, .. } => {
                if let Some(v) = store.get_mut(key) {
                    *v = new_value.clone();
                } else {
                    return Err(WalError::ApplyStore(format!("Update on non-existent key: {}", key)));
                }
            }
            WalEntry::Delete { key, .. } => {
                store.remove(key);
            }
            _ => return Err(WalError::InvalidState("Cannot apply non-operation entry to store".to_string())),
        }
        Ok(())
    }

    pub async fn commit(mut self, db: &Arc<Database>) -> Result<(), WalError> {
        if self.state != TxState::Active {
            return Err(WalError::InvalidState("Cannot commit non-active transaction".to_string()));
        }

        let commit_entry = WalEntry::Commit { 
            tx_id: self.id, 
            timestamp: WalEntry::new_timestamp() 
        };
        db.wal.append(&commit_entry).await?;
        db.wal.flush().await?;

        self.state = TxState::Committed;
        for entry in &self.entries {
            self.apply_to_store(db, entry).await?;
        }
        Ok(())
    }

    pub async fn abort(mut self, db: &Arc<Database>) -> Result<(), WalError> {
        if self.state != TxState::Active {
            return Err(WalError::InvalidState("Cannot abort non-active transaction".to_string()));
        }

        let abort_entry = WalEntry::Abort { 
            tx_id: self.id, 
            timestamp: WalEntry::new_timestamp() 
        };
        db.wal.append(&abort_entry).await?;
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
