use crate::{WalEntry, WalError};
use std::path::Path;
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;

#[cfg(test)]
use tempfile::TempDir;
#[cfg(test)]
use tokio::test as async_test;

#[derive(Clone)]
pub struct WalStorage {
    inner: Arc<Mutex<InnerWal>>,
    pub(crate) path: String,
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
        }
    }

    pub async fn append(&self, entry: &WalEntry) -> Result<(), WalError> {
        let encoded = bincode::serialize(entry)
            .map_err(WalError::Deserialization)?;
        let len = (encoded.len() as u64).to_le_bytes();

        let mut inner = self.inner.lock().await;
        inner.file.write_all(&len).await?;
        inner.file.write_all(&encoded).await?;
        inner.file.flush().await?;
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), WalError> {
        let mut inner = self.inner.lock().await;
        inner.file.get_mut().sync_all().await?;
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
        let entry = WalEntry::Insert {
            tx_id: 1,
            timestamp: 123,
            key: "test".to_string(),
            value: b"val".to_vec(),
        };
        wal.append(&entry).await.unwrap();
        wal.flush().await.unwrap();
        // Verify by reopening and reading (simplified)
        let file = tokio::fs::read(&wal_path).await.unwrap();
        assert!(!file.is_empty());
    }
}
