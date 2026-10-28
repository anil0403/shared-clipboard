//! Bluetooth links between real nodes, with an in-memory pipe for the L2CAP channel.

mod common;

use std::{sync::Arc, time::Duration};

use common::*;
use sharedclip::{BluetoothLink, BluetoothSink, Error, Format};
use tempfile::{TempDir, tempdir};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// A and B, paired, then restarted without each other's addresses, so they can't connect over the network.
async fn apart() -> (Dev, Dev, TempDir, TempDir) {
    let (a, b, da, db) = two().await;
    a.node.stop().await;
    b.node.stop().await;
    drop((a, b));
    for dir in [&da, &db] {
        let peers = dir.path().join("peers.json");
        let mut list: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&peers).unwrap()).unwrap();
        list.iter_mut().for_each(|p| p["addrs"] = serde_json::json!([]));
        std::fs::write(&peers, serde_json::to_vec(&list).unwrap()).unwrap();
    }
    let a = start(da.path(), "A").await;
    let b = start(db.path(), "B").await;
    (a, b, da, db)
}

/// Links `central` and `peripheral` through a pipe, as the Linux app does with its L2CAP socket.
fn link(central: &Dev, peripheral: &Dev) {
    let (x, y) = tokio::io::duplex(64 * 1024);
    central.node.bluetooth_link(x, true);
    peripheral.node.bluetooth_link(y, false);
}

fn over_bluetooth(dev: &Dev, id: &str) -> bool {
    dev.node.devices().iter().any(|d| d.id == id && d.online && d.bluetooth)
}

/// An app's socket: what the core sends goes to the other side's `BluetoothLink`.
struct Socket(UnboundedSender<Option<Vec<u8>>>);

impl BluetoothSink for Socket {
    fn send(&self, data: Vec<u8>) {
        let _ = self.0.send(Some(data));
    }
    fn close_socket(&self) {
        let _ = self.0.send(None);
    }
}

/// Passes what one side sends to the other side's link. Tells `closed` when the sending side closes.
fn forward(mut rx: UnboundedReceiver<Option<Vec<u8>>>, to: Arc<BluetoothLink>, closed: UnboundedSender<()>) {
    tokio::spawn(async move {
        while let Some(Some(data)) = rx.recv().await {
            to.receive(data);
        }
        to.closed();
        let _ = closed.send(());
    });
}

/// Links them through `bluetooth_open`, as the Android app does. Returns a channel that gets a message when a side
/// closes its socket.
fn ffi_link(central: &Dev, peripheral: &Dev) -> UnboundedReceiver<()> {
    let ((ctx, crx), (ptx, prx), (closed_tx, closed)) = (unbounded_channel(), unbounded_channel(), unbounded_channel());
    let c = central.node.bluetooth_open(Arc::new(Socket(ctx)), true);
    let p = peripheral.node.bluetooth_open(Arc::new(Socket(ptx)), false);
    forward(crx, p, closed_tx.clone());
    forward(prx, c, closed_tx);
    closed
}

#[tokio::test(flavor = "multi_thread")]
async fn text_goes_over_bluetooth() {
    let (mut a, mut b, _da, _db) = apart().await;
    assert!(a.node.bluetooth_wanted() && !online(&a, &b.id));
    link(&a, &b);
    wait_until(|| over_bluetooth(&a, &b.id) && over_bluetooth(&b, &a.id)).await;

    a.node.copy(text("over the air"), vec![]).await.unwrap();
    let clip = next(&mut b.clips).await;
    assert_eq!((clip.formats, clip.from), (text("over the air"), a.id.clone()));
    b.node.copy(text("and back"), vec![]).await.unwrap();
    assert_eq!(next(&mut a.clips).await.formats, text("and back"));
    // Still wanted: the network is what it's looking for.
    assert!(a.node.bluetooth_wanted());
}

#[tokio::test(flavor = "multi_thread")]
async fn big_items_and_files_are_offered() {
    let (a, mut b, _da, _db) = apart().await;
    link(&a, &b);
    wait_until(|| over_bluetooth(&a, &b.id) && over_bluetooth(&b, &a.id)).await;

    let big = vec![Format {
        mime: "image/png".into(),
        data: vec![7; 1024 * 1024 + 1],
    }];
    a.node.copy(big, vec![]).await.unwrap();
    let offer = next(&mut b.offers).await;
    assert_eq!(offer.size, 1024 * 1024 + 1);
    // Asking for it over Bluetooth gets `missing`: it waits for a network.
    assert!(matches!(b.node.fetch(offer.id).await, Err(Error::NotFound)));

    let src = tempdir().unwrap();
    let path = src.path().join("small.txt");
    std::fs::write(&path, b"tiny").unwrap();
    let file = sharedclip::File {
        name: "small.txt".into(),
        path: path.to_string_lossy().into_owned(),
        size: 0,
    };
    a.node.copy(vec![], vec![file]).await.unwrap();
    assert_eq!(next(&mut b.offers).await.file_names, vec!["small.txt".to_string()]);
    assert!(b.clips.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bluetooth_api_for_apps_works() {
    let (a, mut b, _da, _db) = apart().await;
    let _closed = ffi_link(&a, &b);
    wait_until(|| over_bluetooth(&a, &b.id) && over_bluetooth(&b, &a.id)).await;
    a.node.copy(text("through the app"), vec![]).await.unwrap();
    assert_eq!(next(&mut b.clips).await.formats, text("through the app"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpaired_device_is_refused() {
    let (a, _b, _da, _db) = two().await;
    let dc = tempdir().unwrap();
    let c = start(dc.path(), "C").await;
    let mut closed = ffi_link(&c, &a);
    next(&mut closed).await;
    assert!(!paired(&a, &c.id) && c.node.devices().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_device_is_refused() {
    let (a, b, _da, _db) = apart().await;
    // B doesn't hear about it: they aren't connected.
    a.node.unpair(b.id.clone()).await.unwrap();
    let mut closed = ffi_link(&b, &a);
    next(&mut closed).await;
    wait_until(|| !online(&b, &a.id)).await;
    assert!(!paired(&a, &b.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn unpair_goes_over_bluetooth() {
    let (a, b, _da, _db) = apart().await;
    link(&a, &b);
    wait_until(|| over_bluetooth(&a, &b.id) && over_bluetooth(&b, &a.id)).await;
    a.node.unpair(b.id.clone()).await.unwrap();
    wait_until(|| !paired(&b, &a.id)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_link_closes() {
    let (a, b, _da, _db) = apart().await;
    // Keepalives (each side's, after 2/5 of its idle time) keep a quiet link going.
    a.node.set_bluetooth_idle(Duration::from_secs(1));
    b.node.set_bluetooth_idle(Duration::from_secs(1));
    link(&a, &b);
    wait_until(|| over_bluetooth(&a, &b.id) && over_bluetooth(&b, &a.id)).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(over_bluetooth(&a, &b.id) && over_bluetooth(&b, &a.id));

    // C's next keepalive is a minute away: D closes the link.
    let (c, d, _dc, _dd) = apart().await;
    d.node.set_bluetooth_idle(Duration::from_millis(500));
    link(&c, &d);
    wait_until(|| over_bluetooth(&c, &d.id) && over_bluetooth(&d, &c.id)).await;
    wait_until(|| !online(&c, &d.id) && !online(&d, &c.id)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_network_is_preferred() {
    let (a, mut b, _da, _db) = two().await;
    link(&a, &b);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(online(&b, &a.id) && !over_bluetooth(&b, &a.id));
    assert!(!a.node.bluetooth_wanted());
    // Too big for Bluetooth, but it's whole: it went over the network.
    let big = vec![Format {
        mime: "image/png".into(),
        data: vec![9; 2 * 1024 * 1024],
    }];
    a.node.copy(big.clone(), vec![]).await.unwrap();
    assert_eq!(next(&mut b.clips).await.formats, big);
    assert!(b.offers.try_recv().is_err());
}
