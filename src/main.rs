use async_wal_db::{Database, WalEntry, WalError};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), WalError> {
    let wal_path = "demo_wal.log";
    let db = Database::new(wal_path).await;
    db.recover().await?;  // Initial recovery

    // Start checkpoint scheduler
    let _scheduler = Arc::clone(&db).start_checkpoint_scheduler(5);  // Every 5s for demo

    // Concurrent transactions
    let db_clone1 = Arc::clone(&db);
    let handle1 = tokio::spawn(async move {
        let mut tx = db_clone1.begin_transaction();
        tx.append_op(&db_clone1, WalEntry::Insert {
            tx_id: 0, timestamp: 0, key: "key1".to_string(), value: b"value1".to_vec()
        }).await.unwrap();
        tx.commit(&db_clone1).await.unwrap();
    });

    let db_clone2 = Arc::clone(&db);
    let handle2 = tokio::spawn(async move {
        let mut tx = db_clone2.begin_transaction();
        tx.append_op(&db_clone2, WalEntry::Insert {
            tx_id: 0, timestamp: 0, key: "key2".to_string(), value: b"value2".to_vec()
        }).await.unwrap();
        tx.commit(&db_clone2).await.unwrap();
    });

    handle1.await.unwrap();
    handle2.await.unwrap();

    // Manual checkpoint
    match db.checkpoint().await {
        Ok(tx_id) => println!("Manual checkpoint completed at TxID: {}", tx_id),
        Err(WalError::NoNewCheckpoints) => println!("No new transactions to checkpoint"),
        Err(e) => return Err(e),
    }

    // Verify
    let store = db.store.read().await;
    println!("Final store keys: {:?}", store.keys().collect::<Vec<_>>());
    println!("Demo complete! Check demo_wal.log and demo_wal.log.checkpoint files.");

    Ok(())
}