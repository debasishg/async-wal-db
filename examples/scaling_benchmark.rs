use async_wal_db::{Database, WalEntry};
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

#[tokio::main]
async fn main() {
    println!("=== Lock-Free WAL Scaling Benchmark ===\n");
    println!("Testing how throughput scales with increasing concurrency\n");
    
    // Test different thread counts
    let thread_counts = vec![1, 2, 4, 8, 16, 32, 64, 128];
    let txns_per_thread = 100;
    
    println!("| Threads | Total Txns | Time (s) | Throughput (txn/s) | Speedup vs 1 | Latency (ms) |");
    println!("|---------+------------+----------+--------------------+--------------+--------------|");
    
    let mut baseline_throughput = 0.0;
    
    for &num_threads in &thread_counts {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join(format!("wal_{}.log", num_threads));
        let db = Database::new(wal_path.to_str().unwrap()).await;
        
        let total_txns = num_threads * txns_per_thread;
        let start = Instant::now();
        let mut handles = vec![];
        
        for thread_id in 0..num_threads {
            let db_clone = Arc::clone(&db);
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
        let throughput = total_txns as f64 / elapsed.as_secs_f64();
        let latency_ms = elapsed.as_secs_f64() * 1000.0 / total_txns as f64;
        
        if num_threads == 1 {
            baseline_throughput = throughput;
        }
        
        let speedup = throughput / baseline_throughput;
        
        println!("| {:7} | {:10} | {:8.2} | {:18.2} | {:12.2}x | {:12.3} |",
            num_threads, total_txns, elapsed.as_secs_f64(), throughput, speedup, latency_ms);
        
        db.shutdown().await.unwrap();
    }
    
    println!("\n=== Batch Size Impact ===\n");
    println!("Testing how batch size affects throughput (10 threads)\n");
    
    let ops_per_txn_values = vec![1, 5, 10, 20, 50];
    let num_threads = 10;
    let base_txns = 50;
    
    println!("| Ops/Txn | Total Ops | Time (s) | Ops Throughput | Txn Latency (ms) |");
    println!("|---------|-----------|----------|----------------|------------------|");
    
    for &ops_per_txn in &ops_per_txn_values {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join(format!("wal_batch_{}.log", ops_per_txn));
        let db = Database::new(wal_path.to_str().unwrap()).await;
        
        let total_txns = num_threads * base_txns;
        let total_ops = total_txns * ops_per_txn;
        let start = Instant::now();
        let mut handles = vec![];
        
        for thread_id in 0..num_threads {
            let db_clone = Arc::clone(&db);
            let handle = tokio::spawn(async move {
                for i in 0..base_txns {
                    let mut tx = db_clone.begin_transaction();
                    for j in 0..ops_per_txn {
                        tx.append_op(&db_clone, WalEntry::Insert {
                            tx_id: 0,
                            timestamp: 0,
                            key: format!("key-{}-{}-{}", thread_id, i, j),
                            value: format!("value-{}-{}-{}", thread_id, i, j).into_bytes(),
                        }).await.unwrap();
                    }
                    tx.commit(&db_clone).await.unwrap();
                }
            });
            handles.push(handle);
        }
        
        for handle in handles {
            handle.await.unwrap();
        }
        
        let elapsed = start.elapsed();
        let ops_throughput = total_ops as f64 / elapsed.as_secs_f64();
        let txn_latency_ms = elapsed.as_secs_f64() * 1000.0 / total_txns as f64;
        
        println!("| {:7} | {:9} | {:8.2} | {:14.2} | {:16.3} |",
            ops_per_txn, total_ops, elapsed.as_secs_f64(), ops_throughput, txn_latency_ms);
        
        db.shutdown().await.unwrap();
    }
    
    println!("\n=== Contention Test ===\n");
    println!("Testing throughput with overlapping vs non-overlapping keys\n");
    
    // Non-overlapping keys (no logical contention)
    let temp_dir1 = TempDir::new().unwrap();
    let wal_path1 = temp_dir1.path().join("wal_no_overlap.log");
    let db1 = Database::new(wal_path1.to_str().unwrap()).await;
    
    let num_threads = 20;
    let txns_per_thread = 50;
    let total_txns = num_threads * txns_per_thread;
    
    let start = Instant::now();
    let mut handles = vec![];
    
    for thread_id in 0..num_threads {
        let db_clone = Arc::clone(&db1);
        let handle = tokio::spawn(async move {
            for i in 0..txns_per_thread {
                let mut tx = db_clone.begin_transaction();
                // Each thread writes to its own key space
                tx.append_op(&db_clone, WalEntry::Insert {
                    tx_id: 0,
                    timestamp: 0,
                    key: format!("thread{}-key{}", thread_id, i),
                    value: b"value".to_vec(),
                }).await.unwrap();
                tx.commit(&db_clone).await.unwrap();
            }
        });
        handles.push(handle);
    }
    
    for handle in handles {
        handle.await.unwrap();
    }
    
    let no_overlap_time = start.elapsed();
    let no_overlap_throughput = total_txns as f64 / no_overlap_time.as_secs_f64();
    
    // Overlapping keys (high logical contention)
    let temp_dir2 = TempDir::new().unwrap();
    let wal_path2 = temp_dir2.path().join("wal_overlap.log");
    let db2 = Database::new(wal_path2.to_str().unwrap()).await;
    
    let start = Instant::now();
    let mut handles = vec![];
    
    for thread_id in 0..num_threads {
        let db_clone = Arc::clone(&db2);
        let handle = tokio::spawn(async move {
            for i in 0..txns_per_thread {
                let mut tx = db_clone.begin_transaction();
                // All threads write to shared keys
                tx.append_op(&db_clone, WalEntry::Insert {
                    tx_id: 0,
                    timestamp: 0,
                    key: format!("shared-key{}", i % 10), // Only 10 shared keys
                    value: b"value".to_vec(),
                }).await.unwrap();
                tx.commit(&db_clone).await.unwrap();
            }
        });
        handles.push(handle);
    }
    
    for handle in handles {
        handle.await.unwrap();
    }
    
    let overlap_time = start.elapsed();
    let overlap_throughput = total_txns as f64 / overlap_time.as_secs_f64();
    
    println!("Non-overlapping keys (no contention):");
    println!("  Time: {:?}", no_overlap_time);
    println!("  Throughput: {:.2} txn/s", no_overlap_throughput);
    
    println!("\nOverlapping keys (high contention):");
    println!("  Time: {:?}", overlap_time);
    println!("  Throughput: {:.2} txn/s", overlap_throughput);
    
    println!("\nContention Impact: {:.2}%", 
        ((no_overlap_throughput - overlap_throughput) / no_overlap_throughput * 100.0));
    
    db1.shutdown().await.unwrap();
    db2.shutdown().await.unwrap();
    
    println!("\n=== Benchmark Complete ===");
}
