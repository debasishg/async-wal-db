use async_wal_db::{Database, WalEntry};
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

#[tokio::main]
async fn main() {
    println!("=== Async WAL Database Benchmark ===\n");
    
    // Benchmark 1: Single-threaded throughput
    println!("📊 Benchmark 1: Single-threaded Write Throughput");
    let temp_dir = TempDir::new().unwrap();
    let wal_path = temp_dir.path().join("bench_single.log").to_str().unwrap().to_string();
    let db = Database::new(&wal_path).await;
    
    let num_txns = 1000;
    let start = Instant::now();
    
    for i in 0..num_txns {
        let mut tx = db.begin_transaction();
        tx.append_op(&db, WalEntry::Insert {
            tx_id: 0,
            timestamp: 0,
            key: format!("key-{}", i),
            value: format!("value-{}", i).into_bytes(),
        }).await.unwrap();
        tx.commit(&db).await.unwrap();
    }
    
    let elapsed = start.elapsed();
    println!("  Transactions: {}", num_txns);
    println!("  Time: {:?}", elapsed);
    println!("  Throughput: {:.2} txns/sec", num_txns as f64 / elapsed.as_secs_f64());
    println!("  Latency: {:.2} ms/txn\n", elapsed.as_secs_f64() * 1000.0 / num_txns as f64);
    
    // Benchmark 2: Concurrent writes (10 threads)
    println!("📊 Benchmark 2: Concurrent Write Throughput (10 threads)");
    let temp_dir2 = TempDir::new().unwrap();
    let wal_path2 = temp_dir2.path().join("bench_concurrent.log").to_str().unwrap().to_string();
    let db2 = Database::new(&wal_path2).await;
    
    let num_threads = 10;
    let txns_per_thread = 100;
    let total_txns = num_threads * txns_per_thread;
    
    let start = Instant::now();
    let mut handles = vec![];
    
    for thread_id in 0..num_threads {
        let db_clone = Arc::clone(&db2);
        let handle = tokio::spawn(async move {
            for i in 0..txns_per_thread {
                let mut tx = db_clone.begin_transaction();
                tx.append_op(&db_clone, WalEntry::Insert {
                    tx_id: 0,
                    timestamp: 0,
                    key: format!("key-{}-{}", thread_id, i),
                    value: format!("value-{}-{}", thread_id, i).into_bytes(),
                }).await.unwrap();
                tx.commit(&db_clone).await.unwrap();
            }
        });
        handles.push(handle);
    }
    
    for handle in handles {
        handle.await.unwrap();
    }
    
    let elapsed = start.elapsed();
    println!("  Threads: {}", num_threads);
    println!("  Total Transactions: {}", total_txns);
    println!("  Time: {:?}", elapsed);
    println!("  Throughput: {:.2} txns/sec", total_txns as f64 / elapsed.as_secs_f64());
    println!("  Latency: {:.2} ms/txn\n", elapsed.as_secs_f64() * 1000.0 / total_txns as f64);
    
    // Benchmark 3: Batch writes
    println!("📊 Benchmark 3: Batch Writes (10 ops/txn)");
    let temp_dir3 = TempDir::new().unwrap();
    let wal_path3 = temp_dir3.path().join("bench_batch.log").to_str().unwrap().to_string();
    let db3 = Database::new(&wal_path3).await;
    
    let num_batch_txns = 100;
    let ops_per_txn = 10;
    let total_ops = num_batch_txns * ops_per_txn;
    
    let start = Instant::now();
    
    for i in 0..num_batch_txns {
        let mut tx = db3.begin_transaction();
        for j in 0..ops_per_txn {
            tx.append_op(&db3, WalEntry::Insert {
                tx_id: 0,
                timestamp: 0,
                key: format!("key-{}-{}", i, j),
                value: format!("value-{}-{}", i, j).into_bytes(),
            }).await.unwrap();
        }
        tx.commit(&db3).await.unwrap();
    }
    
    let elapsed = start.elapsed();
    println!("  Transactions: {}", num_batch_txns);
    println!("  Total Operations: {}", total_ops);
    println!("  Time: {:?}", elapsed);
    println!("  Throughput: {:.2} ops/sec", total_ops as f64 / elapsed.as_secs_f64());
    println!("  Latency: {:.2} ms/txn\n", elapsed.as_secs_f64() * 1000.0 / num_batch_txns as f64);
    
    // Benchmark 4: High concurrency (50 threads)
    println!("📊 Benchmark 4: High Concurrency (50 threads)");
    let temp_dir4 = TempDir::new().unwrap();
    let wal_path4 = temp_dir4.path().join("bench_high_concurrency.log").to_str().unwrap().to_string();
    let db4 = Database::new(&wal_path4).await;
    
    let num_threads_high = 50;
    let txns_per_thread_high = 20;
    let total_txns_high = num_threads_high * txns_per_thread_high;
    
    let start = Instant::now();
    let mut handles = vec![];
    
    for thread_id in 0..num_threads_high {
        let db_clone = Arc::clone(&db4);
        let handle = tokio::spawn(async move {
            for i in 0..txns_per_thread_high {
                let mut tx = db_clone.begin_transaction();
                tx.append_op(&db_clone, WalEntry::Insert {
                    tx_id: 0,
                    timestamp: 0,
                    key: format!("key-{}-{}", thread_id, i),
                    value: format!("value-{}-{}", thread_id, i).into_bytes(),
                }).await.unwrap();
                tx.commit(&db_clone).await.unwrap();
            }
        });
        handles.push(handle);
    }
    
    for handle in handles {
        handle.await.unwrap();
    }
    
    let elapsed = start.elapsed();
    println!("  Threads: {}", num_threads_high);
    println!("  Total Transactions: {}", total_txns_high);
    println!("  Time: {:?}", elapsed);
    println!("  Throughput: {:.2} txns/sec", total_txns_high as f64 / elapsed.as_secs_f64());
    println!("  Latency: {:.2} ms/txn\n", elapsed.as_secs_f64() * 1000.0 / total_txns_high as f64);
    
    println!("=== Benchmark Complete ===");
    
    // Graceful shutdown
    db.shutdown().await.unwrap();
    db2.shutdown().await.unwrap();
    db3.shutdown().await.unwrap();
    db4.shutdown().await.unwrap();
}
