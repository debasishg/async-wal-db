# Phase 2: LMDB Integration (TODO)

## Overview

Integrate LMDB (Lightning Memory-Mapped Database) as the storage backend to replace the in-memory HashMap, enabling:
- Zero-copy reads via memory mapping (mmap)
- Persistent storage with crash recovery
- Blazing fast read performance (~nanoseconds for cached data)
- Efficient B+tree storage on disk

## Current Status

✅ **Phase 1 Complete**: Lock-Free WAL
- Lock-free concurrent writes using `crossbeam::SegQueue`
- Background flusher with configurable interval (default 10ms)
- 3.7x throughput improvement with 50 concurrent threads
- All tests passing

✅ **LMDB Module Created**: `src/lmdb_storage.rs`
- Basic CRUD operations
- Batch write support
- Zero-copy reads

🔄 **Integration In Progress**: Partially started, needs completion

## Architecture Design

### Current (Phase 1)
```
┌──────────────────────────────────────┐
│  Lock-Free WAL (SegQueue)            │ ← Fast concurrent writes
│  Background Flusher → wal.log        │
└─────────────┬────────────────────────┘
              │
              ▼
┌──────────────────────────────────────┐
│  HashMap (In-Memory)                 │ ← Lost on restart
└──────────────────────────────────────┘
```

### Target (Phase 2)
```
┌──────────────────────────────────────┐
│  Lock-Free WAL (SegQueue)            │ ← Fast concurrent writes
│  Background Flusher → wal.log        │
└─────────────┬────────────────────────┘
              │
              │ Checkpoint (batched)
              ▼
┌──────────────────────────────────────┐
│  LMDB (mmap, on-disk)                │ ← Zero-copy reads, persistent
│  B+tree storage                      │
└──────────────────────────────────────┘
```

### Flow
1. **Writes**: Transaction → Lock-free WAL → Background flush to disk
2. **Checkpoint**: Batch-apply WAL entries → LMDB (single transaction)
3. **Reads**: Check WAL first, then LMDB (zero-copy)
4. **Recovery**: Load LMDB state + replay uncommitted WAL entries

## Implementation Tasks

### 1. Update `database.rs`

#### Current Signature
```rust
pub async fn new(wal_path: &str) -> Arc<Self>
```

#### New Signature
```rust
pub async fn new(wal_path: &str, db_path: &str) -> Arc<Self>
```

#### Update Recovery Method
```rust
pub async fn recover(&self) -> Result<(), WalError> {
    // Read all WAL entries
    let (tx_entries, committed_txs) = self.read_wal().await?;
    
    // Batch-write to LMDB in spawn_blocking
    let store = Arc::clone(&self.store);
    let sorted_tx_ids: Vec<u64> = committed_txs.iter().copied().collect();
    
    tokio::task::spawn_blocking(move || {
        store.batch_write(|wtxn, db| {
            for tx_id in sorted_tx_ids {
                if let Some(entries) = tx_entries.get(&tx_id) {
                    for entry in entries {
                        match entry {
                            WalEntry::Insert { key, value, .. } => {
                                db.put(wtxn, key, value)?;
                            }
                            WalEntry::Update { key, new_value, .. } => {
                                db.put(wtxn, key, new_value)?;
                            }
                            WalEntry::Delete { key, .. } => {
                                db.delete(wtxn, key)?;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(())
        })
    }).await??;
    
    Ok(())
}
```

#### Update Checkpoint Method
```rust
pub async fn checkpoint(&self) -> Result<u64, WalError> {
    // Read WAL entries since last checkpoint
    let (tx_entries, committed_txs) = self.read_wal_since(last_ckpt).await?;
    
    // Batch-apply to LMDB
    let store = Arc::clone(&self.store);
    tokio::task::spawn_blocking(move || {
        store.batch_write(|wtxn, db| {
            // Apply all committed transactions in order
            for tx_id in sorted_tx_ids {
                // ... apply entries ...
            }
            Ok(())
        })
    }).await??;
    
    Ok(max_ckpt)
}
```

### 2. Update `transaction.rs`

#### Update `apply_to_store` Method
```rust
pub async fn apply_to_store(&self, db: &Arc<Database>, op: &WalEntry) -> Result<(), WalError> {
    let store = Arc::clone(&db.store);
    let op = op.clone();
    
    tokio::task::spawn_blocking(move || {
        match op {
            WalEntry::Insert { key, value, .. } => {
                store.put(&key, &value)?;
            }
            WalEntry::Update { key, new_value, .. } => {
                store.put(&key, &new_value)?;
            }
            WalEntry::Delete { key, .. } => {
                store.delete(&key)?;
            }
            _ => {}
        }
        Ok(())
    }).await?
}
```

### 3. Update All Tests

Every test needs to provide both `wal_path` and `db_path`:

```rust
#[async_test]
async fn test_recovery() {
    let temp_dir = TempDir::new().unwrap();
    let wal_path = temp_dir.path().join("wal.log").to_str().unwrap().to_string();
    let db_path = temp_dir.path().join("db.mdb").to_str().unwrap().to_string();
    let db = Database::new(&wal_path, &db_path).await;
    // ... rest of test
}
```

### 4. Update `main.rs` and Examples

```rust
let db = Database::new("data/wal.log", "data/db.mdb").await;
```

### 5. Update Benchmark

```rust
let temp_dir = TempDir::new().unwrap();
let wal_path = temp_dir.path().join("bench.wal");
let db_path = temp_dir.path().join("bench.mdb");
let db = Database::new(
    wal_path.to_str().unwrap(),
    db_path.to_str().unwrap()
).await;
```

## Performance Expectations

### Read Performance
- **HashMap**: ~50ns (in-memory hash lookup)
- **LMDB**: ~100ns (mmap, OS page cache hit)
- **Trade-off**: Slightly slower reads, but persistent

### Write Performance
- **Unchanged**: Lock-free WAL is same regardless of backend
- **Checkpoint**: Batched LMDB writes more efficient than individual

### Memory Usage
- **HashMap**: All data in RAM (GBs for large datasets)
- **LMDB**: Only working set in RAM, rest on disk

### Durability
- **HashMap**: Lost on crash (relies on WAL replay)
- **LMDB**: Persisted after checkpoint

## Configuration Options

### LMDB Environment Size
```rust
EnvOpenOptions::new()
    .map_size(10 * 1024 * 1024 * 1024) // 10GB max
    .max_dbs(1)
    .open(path)?
```

Tune based on dataset size.

### Checkpoint Interval
```rust
db.start_checkpoint_scheduler(60); // Every 60 seconds
```

More frequent = less WAL replay on recovery, but more I/O.

## Testing Strategy

1. **Unit Tests**: Verify LMDB operations work correctly
2. **Integration Tests**: Ensure recovery works end-to-end
3. **Benchmark**: Compare HashMap vs LMDB performance
4. **Stress Test**: Large dataset (millions of keys)

## Rollback Plan

If LMDB integration causes issues:
1. Revert to `database.rs.hashmap_backup`
2. Keep lock-free WAL improvements
3. Phase 2 becomes optional feature

## Estimated Effort

- Implementation: 2-3 hours
- Testing: 1 hour
- Benchmarking: 30 minutes
- **Total**: ~4 hours

## Benefits vs Risks

### Benefits ✅
- Persistent storage (no data loss on restart)
- Zero-copy reads (performance)
- Handles datasets larger than RAM
- Production-grade storage (used by OpenLDAP, Bitcoin Core)

### Risks ⚠️
- Complexity increase
- Need to tune LMDB parameters
- Slightly slower reads (mmap vs HashMap)
- Single-writer limitation (checkpoint only)

## Decision

**Defer to later**: Phase 1 (lock-free WAL) already provides significant improvements. Phase 2 can be implemented when:
- Need persistent storage across restarts
- Dataset exceeds available RAM
- Want production deployment

## Files Modified

- ✅ `Cargo.toml` - Add heed dependency
- ✅ `src/lmdb_storage.rs` - Created
- ✅ `src/lib.rs` - Export LmdbStorage
- 🔄 `src/database.rs` - Partially updated (rollback available)
- ⏳ `src/transaction.rs` - Not started
- ⏳ `src/main.rs` - Not started
- ⏳ `examples/benchmark.rs` - Not started
- ⏳ All test files - Not started

## Next Steps When Ready

1. Complete database.rs refactoring
2. Update transaction.rs
3. Fix all tests
4. Run benchmarks
5. Compare HashMap vs LMDB performance
6. Document findings
