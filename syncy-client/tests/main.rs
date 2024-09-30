use rand::RngCore;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use syncy_client::repo::DocRepo;
use tokio::sync::Notify;
use tokio::time::{timeout, Instant};
use uuid::{NoContext, Timestamp, Uuid};
use yrs::{AsyncTransact, Map, ReadTxn, WriteTxn};

const URL: &'static str = "ws://localhost:8080/docs/";

#[tokio::test]
async fn one_update() {
    let _ = env_logger::builder().is_test(true).try_init();

    let doc_id = Uuid::new_v7(Timestamp::now(NoContext));
    let p1 = DocRepo::new(URL, 1).await.unwrap();
    let p2 = DocRepo::new(URL, 2).await.unwrap();

    // setup awaiter on peer 2
    let notify = {
        let d2 = p2.load(doc_id).unwrap();
        let notify = Arc::new(Notify::new());
        let n = notify.clone();
        d2.observe_update_v1_with("test-awaiter", move |_, _| n.notify_waiters())
            .unwrap();
        notify
    };

    // make a change on peer 1
    {
        let d1 = p1.load(doc_id).unwrap();
        let client_id = d1.client_id();
        let mut tx = d1.transact_mut_with(client_id).await;
        let map = tx.get_or_insert_map("map");
        map.insert(&mut tx, "key", "value");
    }

    // wait for update and confirm that change was reached
    timeout(Duration::from_secs(1), notify.notified())
        .await
        .unwrap();

    let d2 = p2.load(doc_id).unwrap();
    let tx = d2.transact().await;
    let map = tx.get_map("map").unwrap();
    let value = map.get(&tx, "key");

    assert_eq!(value, Some("value".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn big_update() {
    let _ = env_logger::builder().is_test(true).try_init();

    const UPDATE_SIZE: usize = 50 * 1024 * 1024; // 50MiB
    let mut value = vec![0u8; UPDATE_SIZE];
    rand::thread_rng().fill_bytes(&mut value);

    let doc_id = Uuid::new_v7(Timestamp::now(NoContext));
    let p1 = DocRepo::new(URL, 1).await.unwrap();
    let p2 = DocRepo::new(URL, 2).await.unwrap();

    // setup awaiter on peer 2
    let notify = {
        let d2 = p2.load(doc_id).unwrap();
        let notify = Arc::new(Notify::new());
        let n = notify.clone();
        d2.observe_update_v1_with("test-awaiter", move |_, _| n.notify_waiters())
            .unwrap();
        notify
    };

    // make a change on peer 1
    let start = Instant::now();
    {
        let d1 = p1.load(doc_id).unwrap();
        let client_id = d1.client_id();
        let mut tx = d1.transact_mut_with(client_id).await;
        let map = tx.get_or_insert_map("map");
        map.insert(&mut tx, "key", value.clone());
    }

    // wait for update and confirm that change was reached
    timeout(Duration::from_secs(5), notify.notified())
        .await
        .unwrap();
    println!("sync completed in {:?}", start.elapsed());

    let d2 = p2.load(doc_id).unwrap();
    let tx = d2.transact().await;
    let map = tx.get_map("map").unwrap();
    let actual = map.get(&tx, "key");

    assert_eq!(actual, Some(value.into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lot_of_updates() {
    let _ = env_logger::builder().is_test(true).try_init();
    const UPDATE_COUNT: u32 = 100_000;
    let doc_id = Uuid::new_v7(Timestamp::now(NoContext));
    let p1 = DocRepo::new(URL, 1).await.unwrap();
    let p2 = DocRepo::new(URL, 2).await.unwrap();

    // setup awaiter on peer 2
    let notify = {
        let d2 = p2.load(doc_id).unwrap();
        let notify = Arc::new(Notify::new());
        let n = notify.clone();
        let counter = AtomicU32::new(0);
        d2.observe_update_v1_with("test-awaiter", move |_, _| {
            let prev = counter.fetch_add(1, Ordering::SeqCst);
            if prev + 1 == UPDATE_COUNT {
                n.notify_waiters()
            }
        })
        .unwrap();
        notify
    };

    // make a change on peer 1
    let start = Instant::now();
    {
        let d1 = p1.load(doc_id).unwrap();
        let client_id = d1.client_id();
        for i in 0..=UPDATE_COUNT {
            let mut tx = d1.transact_mut_with(client_id).await;
            let map = tx.get_or_insert_map("map");
            map.insert(&mut tx, "key", i);
        }
    }

    // wait for update and confirm that change was reached
    timeout(Duration::from_secs(10), notify.notified())
        .await
        .unwrap();
    println!("sync completed in {:?}", start.elapsed());

    let d2 = p2.load(doc_id).unwrap();
    let tx = d2.transact().await;
    let map = tx.get_map("map").unwrap();
    let actual = map.get(&tx, "key");

    assert_eq!(actual, Some(UPDATE_COUNT.into()));
}
