mod wal_storage;
mod transaction;
mod database;

pub use wal_storage::WalStorage;
pub use transaction::{Transaction, TxState};
pub use database::Database;

use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicU64;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum WalError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),
    #[error("Deserialization error: {0}")]
    Deserialization(bincode::Error),
    #[error("Failed to open WAL")]
    OpenWal,
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Transaction already finalized")]
    AlreadyFinalized,
    #[error("No new checkpoints available")]
    NoNewCheckpoints,
    #[error("Checkpoint error: {0}")]
    Checkpoint(String),
    #[error("Invalid state: {0}")]
    InvalidState(String),
    #[error("Apply to store failed: {0}")]
    ApplyStore(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WalEntry {
    Insert {
        tx_id: u64,
        timestamp: u64,
        key: String,
        value: Vec<u8>,
    },
    Update {
        tx_id: u64,
        timestamp: u64,
        key: String,
        old_value: Vec<u8>,
        new_value: Vec<u8>,
    },
    Delete {
        tx_id: u64,
        timestamp: u64,
        key: String,
        old_value: Vec<u8>,
    },
    Commit {
        tx_id: u64,
        timestamp: u64,
    },
    Abort {
        tx_id: u64,
        timestamp: u64,
    },
}

pub(crate) static NEXT_TX_ID: AtomicU64 = AtomicU64::new(1);

impl WalEntry {
    pub fn new_timestamp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wal_entry_serialization() {
        let entry = WalEntry::Insert {
            tx_id: 1,
            timestamp: 123456,
            key: "test_key".to_string(),
            value: b"test_value".to_vec(),
        };
        let serialized = bincode::serialize(&entry).unwrap();
        let deserialized: WalEntry = bincode::deserialize(&serialized).unwrap();
        assert_eq!(entry, deserialized);
    }
}
