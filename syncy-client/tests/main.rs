use std::sync::Arc;
use std::time::Duration;
use syncy_client::repo::DocRepo;
use tokio::sync::Notify;
use tokio::time::timeout;
use uuid::{NoContext, Timestamp, Uuid};
use yrs::{AsyncTransact, Map, ReadTxn, WriteTxn};

const URL: &'static str = "ws://localhost:8080/docs/";

#[tokio::test]
async fn network_controller_roundtrip() {
    let _ = env_logger::builder().is_test(true).try_init();

    let doc_id = Uuid::new_v7(Timestamp::now(NoContext));
    let p1 = DocRepo::new(URL, 1).await.unwrap();
    let p2 = DocRepo::new(URL, 2).await.unwrap();

    // setup awaiter on peer 2
    let notify = {
        let d2 = p2.doc(doc_id).unwrap();
        let notify = Arc::new(Notify::new());
        let n = notify.clone();
        d2.observe_update_v1_with("test-awaiter", move |_, _| n.notify_waiters())
            .unwrap();
        notify
    };

    // make a change on peer 1
    {
        let d1 = p1.doc(doc_id).unwrap();
        let client_id = d1.client_id();
        let mut tx = d1.transact_mut_with(client_id).await;
        let map = tx.get_or_insert_map("map");
        map.insert(&mut tx, "key", "value");
    }

    // wait for update and confirm that change was reached
    timeout(Duration::from_secs(1), notify.notified())
        .await
        .unwrap();

    let d2 = p2.doc(doc_id).unwrap();
    let tx = d2.transact().await;
    let map = tx.get_map("map").unwrap();
    let value = map.get(&tx, "key");

    assert_eq!(value, Some("value".into()));
}
