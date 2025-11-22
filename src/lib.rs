mod wal_storage;
mod transaction;
mod database;
mod lmdb_storage;

pub use wal_storage::WalStorage;
pub use transaction::{Transaction, TxState};
pub use database::{Database, DatabaseConfig};
pub use lmdb_storage::LmdbStorage;

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
    #[error("Checksum mismatch: expected {expected:#x}, got {actual:#x}")]
    ChecksumMismatch { expected: u32, actual: u32 },
    #[error("Partial write detected at offset {offset}, truncating WAL tail")]
    PartialWrite { offset: u64 },
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

/// Recovery action taken for a transaction during crash recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Transaction was committed successfully
    Commit,
    /// Transaction was explicitly aborted
    Rollback,
    /// Transaction was incomplete (no Commit/Abort marker)
    Incomplete,
}

/// Statistics collected during recovery process.
#[derive(Debug, Default, Clone)]
pub struct RecoveryStats {
    /// Total transactions found in WAL
    pub total_transactions: usize,
    /// Transactions that were committed
    pub committed: usize,
    /// Transactions that were aborted
    pub aborted: usize,
    /// Incomplete transactions (missing Commit/Abort)
    pub incomplete: usize,
    /// Partial writes detected and truncated
    pub partial_writes: usize,
    /// Checksum validation failures
    pub checksum_failures: usize,
}

/// WAL entry header containing metadata for integrity checking.
/// 
/// Format on disk:
/// ```text
/// [length: u64][checksum: u32][version: u8][entry_data: bytes]
/// ```
#[derive(Debug, Clone, Copy)]
pub struct WalEntryHeader {
    /// Length of the serialized entry data (not including header)
    pub length: u64,
    /// CRC32 checksum of the serialized entry data
    pub checksum: u32,
    /// Format version (currently always 1)
    pub version: u8,
}

impl WalEntryHeader {
    /// Current WAL format version
    pub const VERSION: u8 = 1;
    
    /// Size of header in bytes (8 + 4 + 1 = 13)
    pub const SIZE: usize = 13;
    
    /// Creates a new header for the given serialized entry data.
    pub fn new(data: &[u8]) -> Self {
        let checksum = crc32fast::hash(data);
        Self {
            length: data.len() as u64,
            checksum,
            version: Self::VERSION,
        }
    }
    
    /// Serializes the header to bytes.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut bytes = [0u8; Self::SIZE];
        bytes[0..8].copy_from_slice(&self.length.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.checksum.to_le_bytes());
        bytes[12] = self.version;
        bytes
    }
    
    /// Deserializes a header from bytes.
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let length = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let checksum = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let version = bytes[12];
        Self { length, checksum, version }
    }
    
    /// Validates that the data matches the header's checksum.
    pub fn validate(&self, data: &[u8]) -> Result<(), WalError> {
        let actual_checksum = crc32fast::hash(data);
        if actual_checksum != self.checksum {
            return Err(WalError::ChecksumMismatch {
                expected: self.checksum,
                actual: actual_checksum,
            });
        }
        Ok(())
    }
}

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
            timestamp: 123_456,
            key: "test_key".to_string(),
            value: b"test_value".to_vec(),
        };
        let serialized = bincode::serialize(&entry).unwrap();
        let deserialized: WalEntry = bincode::deserialize(&serialized).unwrap();
        assert_eq!(entry, deserialized);
    }
}
