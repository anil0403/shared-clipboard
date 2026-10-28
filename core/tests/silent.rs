//! A device that goes silent (out of range, Wi-Fi off) without closing its connection.

mod common;

use std::time::Duration;

use common::*;
use tempfile::tempdir;

#[tokio::test(flavor = "multi_thread")]
async fn network_change_drops_a_device_that_went_silent() {
    let da = tempdir().unwrap();
    let a = start(da.path(), "A").await;
    // B runs on its own single-threaded runtime, so blocking that thread stops every packet B would send.
    let (tx, rx) = std::sync::mpsc::channel();
    let (freeze_tx, freeze_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = tempdir().unwrap();
            let b = start(db.path(), "B").await;
            tx.send((b.node.clone(), b.id.clone())).unwrap();
            while freeze_rx.try_recv().is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            std::thread::sleep(Duration::from_secs(30));
        });
    });
    let (b_node, b_id) = rx.recv().unwrap();
    b_node.pair_link(a.node.pairing_link()).await.unwrap();
    wait_until(|| online(&a, &b_id)).await;

    // A device that's there stays online.
    a.node.network_change().await;
    assert!(online(&a, &b_id));

    freeze_tx.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.node.network_change().await;
    wait_until(|| !online(&a, &b_id)).await;
}
