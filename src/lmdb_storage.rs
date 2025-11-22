use crate::WalError;
use heed::{Database, Env, EnvOpenOptions};
use heed::types::{Str, Bytes};
use std::path::Path;
use std::sync::Arc;

/// LMDB-backed storage for zero-copy reads
pub struct LmdbStorage {
    env: Arc<Env>,
    db: Database<Str, Bytes>,
}

impl LmdbStorage {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, WalError> {
        std::fs::create_dir_all(&path).map_err(WalError::Io)?;
        
        // Create LMDB environment with 1GB max size
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(1024 * 1024 * 1024) // 1GB
                .max_dbs(1)
                .open(path)
                .map_err(|e| WalError::Io(std::io::Error::other(
                    format!("Failed to open LMDB: {e}")
                )))?
        };

        // Create/open the main database
        let mut wtxn = env.write_txn().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to create write transaction: {e}")
        )))?;
        
        let db = env.create_database(&mut wtxn, None).map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to create database: {e}")
        )))?;
        
        wtxn.commit().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to commit initial transaction: {e}")
        )))?;

        Ok(Self {
            env: Arc::new(env),
            db,
        })
    }

    /// Get a value - zero-copy read via mmap
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, WalError> {
        let rtxn = self.env.read_txn().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to create read transaction: {e}")
        )))?;
        
        let result = self.db.get(&rtxn, key).map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to get value: {e}")
        )))?;
        
        Ok(result.map(<[u8]>::to_vec))
    }

    /// Put a value
    pub fn put(&self, key: &str, value: &[u8]) -> Result<(), WalError> {
        let mut wtxn = self.env.write_txn().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to create write transaction: {e}")
        )))?;
        
        self.db.put(&mut wtxn, key, value).map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to put value: {e}")
        )))?;
        
        wtxn.commit().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to commit transaction: {e}")
        )))?;
        
        Ok(())
    }

    /// Delete a key
    pub fn delete(&self, key: &str) -> Result<bool, WalError> {
        let mut wtxn = self.env.write_txn().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to create write transaction: {e}")
        )))?;
        
        let deleted = self.db.delete(&mut wtxn, key).map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to delete key: {e}")
        )))?;
        
        wtxn.commit().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to commit transaction: {e}")
        )))?;
        
        Ok(deleted)
    }

    /// Batch write - apply multiple operations in single transaction
    pub fn batch_write<F>(&self, f: F) -> Result<(), WalError>
    where
        F: FnOnce(&mut heed::RwTxn, &Database<Str, Bytes>) -> Result<(), WalError>,
    {
        let mut wtxn = self.env.write_txn().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to create write transaction: {e}")
        )))?;
        
        f(&mut wtxn, &self.db)?;
        
        wtxn.commit().map_err(|e| WalError::Io(std::io::Error::other(
            format!("Failed to commit batch transaction: {e}")
        )))?;
        
        Ok(())
    }

    /// Get environment for advanced operations
    pub fn env(&self) -> &Arc<Env> {
        &self.env
    }

    /// Get database handle
    pub fn db(&self) -> &Database<Str, Bytes> {
        &self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_lmdb_basic_ops() {
        let temp_dir = TempDir::new().unwrap();
        let lmdb_path = temp_dir.path().join("test.mdb");
        let storage = LmdbStorage::new(&lmdb_path).unwrap();

        // Put
        storage.put("key1", b"value1").unwrap();
        
        // Get
        let value = storage.get("key1").unwrap();
        assert_eq!(value, Some(b"value1".to_vec()));

        // Delete
        let deleted = storage.delete("key1").unwrap();
        assert!(deleted);
        
        let value = storage.get("key1").unwrap();
        assert_eq!(value, None);
    }

    #[test]
    fn test_lmdb_batch_write() {
        let temp_dir = TempDir::new().unwrap();
        let lmdb_path = temp_dir.path().join("test_batch.mdb");
        let storage = LmdbStorage::new(&lmdb_path).unwrap();

        // Batch write
        storage.batch_write(|wtxn, db| {
            db.put(wtxn, "key1", b"value1").map_err(|e| WalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Batch put failed: {}", e)
            )))?;
            db.put(wtxn, "key2", b"value2").map_err(|e| WalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Batch put failed: {}", e)
            )))?;
            Ok(())
        }).unwrap();

        // Verify
        assert_eq!(storage.get("key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(storage.get("key2").unwrap(), Some(b"value2".to_vec()));
    }
}
