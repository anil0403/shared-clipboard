//! Real nodes on this machine, talking over iroh, each in its own temp dir.

use std::{path::Path, sync::Arc, time::Duration};

use sharedclip::{Clip, Config, Device, Error, Events, File, Format, Node, Offer, Platform, SIZE_CAP};
use tempfile::{TempDir, tempdir};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

struct Recorder {
    clips: UnboundedSender<Clip>,
    offers: UnboundedSender<Offer>,
    devices: UnboundedSender<Vec<Device>>,
    codes: UnboundedSender<(Device, String)>,
}

impl Events for Recorder {
    fn on_clip(&self, clip: Clip) {
        let _ = self.clips.send(clip);
    }
    fn on_offer(&self, offer: Offer) {
        let _ = self.offers.send(offer);
    }
    fn on_devices(&self, devices: Vec<Device>) {
        let _ = self.devices.send(devices);
    }
    fn on_pairing_code(&self, device: Device, code: String) {
        let _ = self.codes.send((device, code));
    }
}

struct Dev {
    node: Arc<Node>,
    id: String,
    clips: UnboundedReceiver<Clip>,
    offers: UnboundedReceiver<Offer>,
    devices: UnboundedReceiver<Vec<Device>>,
    codes: UnboundedReceiver<(Device, String)>,
}

async fn start(dir: &Path, name: &str) -> Dev {
    start_on(dir, name, 0).await
}

async fn start_on(dir: &Path, name: &str, port: u16) -> Dev {
    let (clips_tx, clips) = unbounded_channel();
    let (offers_tx, offers) = unbounded_channel();
    let (devices_tx, devices) = unbounded_channel();
    let (codes_tx, codes) = unbounded_channel();
    let events = Recorder {
        clips: clips_tx,
        offers: offers_tx,
        devices: devices_tx,
        codes: codes_tx,
    };
    let config = Config {
        data_dir: dir.to_string_lossy().into_owned(),
        name: name.into(),
        platform: Platform::Linux,
        port,
    };
    let node = Node::start(config, Arc::new(events)).await.unwrap();
    Dev {
        id: node.id(),
        node,
        clips,
        offers,
        devices,
        codes,
    }
}

async fn within<T>(what: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), what)
        .await
        .expect("timed out")
}

async fn next<T>(rx: &mut UnboundedReceiver<T>) -> T {
    within(rx.recv()).await.unwrap()
}

async fn wait_until(mut ok: impl FnMut() -> bool) {
    within(async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
}

/// Nothing more arrives for a moment.
async fn quiet<T: std::fmt::Debug>(rx: &mut UnboundedReceiver<T>) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    if let Ok(got) = rx.try_recv() {
        panic!("didn't expect {got:?}");
    }
}

fn online(dev: &Dev, id: &str) -> bool {
    dev.node.devices().iter().any(|d| d.id == id && d.online)
}

fn paired(dev: &Dev, id: &str) -> bool {
    dev.node.devices().iter().any(|d| d.id == id)
}

fn text(s: &str) -> Vec<Format> {
    vec![Format {
        mime: "text/plain;charset=utf-8".into(),
        data: s.as_bytes().to_vec(),
    }]
}

fn png(byte: u8) -> Vec<Format> {
    vec![Format {
        mime: "image/png".into(),
        data: vec![byte; 2000],
    }]
}

/// `joiner` scans `host`'s link; waits until they're connected.
async fn pair(host: &Dev, joiner: &Dev) {
    let device = joiner.node.pair_link(host.node.pairing_link()).await.unwrap();
    assert_eq!(device.id, host.id);
    wait_until(|| online(host, &joiner.id) && online(joiner, &host.id)).await;
}

async fn two() -> (Dev, Dev, TempDir, TempDir) {
    let (da, db) = (tempdir().unwrap(), tempdir().unwrap());
    let a = start(da.path(), "A").await;
    let b = start(db.path(), "B").await;
    pair(&a, &b).await;
    (a, b, da, db)
}

#[tokio::test(flavor = "multi_thread")]
async fn link_pairing_then_items_arrive() {
    let (a, mut b, _da, db) = two().await;
    let devices = next(&mut b.devices).await;
    assert!(devices.iter().any(|d| d.id == a.id && d.name == "A"));

    a.node.copy(text("hello"), vec![]).await.unwrap();
    let clip = next(&mut b.clips).await;
    assert_eq!((clip.formats, clip.from.as_str()), (text("hello"), a.id.as_str()));
    assert_eq!(b.node.history()[0].id, clip.id);
    assert_eq!(a.node.history()[0].id, clip.id);

    // A file is saved under B's data dir, with a safe name.
    let src = tempdir().unwrap();
    let path = src.path().join("notes.txt");
    std::fs::write(&path, b"file body").unwrap();
    let file = File {
        name: "../notes.txt".into(),
        path: path.to_string_lossy().into_owned(),
        size: 0,
    };
    a.node.copy(vec![], vec![file]).await.unwrap();
    let clip = next(&mut b.clips).await;
    let got = &clip.files[0];
    assert_eq!((got.name.as_str(), got.size), ("../notes.txt", 9));
    assert!(
        got.path
            .starts_with(db.path().join("files").join(&clip.id).to_str().unwrap())
    );
    assert!(got.path.ends_with(".._notes.txt"));
    assert_eq!(std::fs::read(&got.path).unwrap(), b"file body");
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_is_not_sent_back() {
    let (mut a, mut b, _da, _db) = two().await;
    a.node.copy(text("one"), vec![]).await.unwrap();
    let clip = next(&mut b.clips).await;
    // B's app puts it on the clipboard, and its watcher reports it.
    b.node.copy(clip.formats, clip.files).await.unwrap();
    b.node.copy(text("two"), vec![]).await.unwrap();
    assert_eq!(next(&mut a.clips).await.formats, text("two"));
    quiet(&mut a.clips).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn big_items_are_offered_then_fetched() {
    let (a, mut b, _da, _db) = two().await;
    let size = SIZE_CAP + 1_000_000;
    let big = vec![Format {
        mime: "application/octet-stream".into(),
        data: (0..size).map(|i| i as u8).collect(),
    }];
    a.node.copy(big.clone(), vec![]).await.unwrap();
    let offer = next(&mut b.offers).await;
    assert_eq!((offer.size, offer.from.as_str()), (size, a.id.as_str()));
    assert!(b.clips.try_recv().is_err());

    let clip = within(b.node.fetch(offer.id.clone())).await.unwrap();
    assert_eq!((clip.id.as_str(), clip.formats == big), (offer.id.as_str(), true));
    assert_eq!(b.node.history()[0].id, offer.id);
    assert!(matches!(b.node.fetch("00".repeat(32)).await, Err(Error::NotFound)));
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_fails_when_the_device_goes_away() {
    let (a, mut b, _da, _db) = two().await;
    let big = vec![Format {
        mime: "application/octet-stream".into(),
        data: vec![7; SIZE_CAP as usize + 1],
    }];
    a.node.copy(big, vec![]).await.unwrap();
    let offer = next(&mut b.offers).await;

    // A doesn't answer (it's paused), then goes away while B waits.
    a.node.set_paused(true);
    let node = b.node.clone();
    let fetch = tokio::spawn(async move { node.fetch(offer.id).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.node.stop().await;
    assert!(within(fetch).await.unwrap().is_err());
}

/// B found A through discovery and pairs by comparing codes.
async fn code_pairing(accept_on_a: bool) -> (Dev, Dev, Result<Device, Error>, [TempDir; 2]) {
    let (da, db) = (tempdir().unwrap(), tempdir().unwrap());
    let mut a = start(da.path(), "A").await;
    let mut b = start(db.path(), "B").await;
    b.node.found(a.id.clone(), format!("127.0.0.1:{}", a.node.port()));
    let (node, id) = (b.node.clone(), a.id.clone());
    let pairing = tokio::spawn(async move { node.pair_device(id).await });

    let (b_seen_by_a, code_a) = next(&mut a.codes).await;
    let (a_seen_by_b, code_b) = next(&mut b.codes).await;
    assert_eq!(code_a, code_b);
    assert_eq!(code_a.len(), 6);
    assert_eq!((b_seen_by_a.id, b_seen_by_a.name), (b.id.clone(), "B".to_string()));
    assert_eq!((a_seen_by_b.id, a_seen_by_b.name), (a.id.clone(), "A".to_string()));

    a.node.confirm_pairing(b.id.clone(), accept_on_a);
    b.node.confirm_pairing(a.id.clone(), true);
    let result = within(pairing).await.unwrap();
    (a, b, result, [da, db])
}

#[tokio::test(flavor = "multi_thread")]
async fn code_pairing_confirmed() {
    let (a, b, result, _dirs) = code_pairing(true).await;
    assert_eq!(result.unwrap().id, a.id);
    wait_until(|| online(&a, &b.id) && online(&b, &a.id)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn code_pairing_rejected() {
    let (a, b, result, _dirs) = code_pairing(false).await;
    assert!(matches!(result, Err(Error::PairingRejected)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a.node.devices().is_empty() && b.node.devices().is_empty());
}

async fn three() -> (Dev, Dev, Dev, [TempDir; 3]) {
    let dirs = [tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap()];
    let a = start(dirs[0].path(), "A").await;
    let b = start(dirs[1].path(), "B").await;
    let c = start(dirs[2].path(), "C").await;
    pair(&a, &b).await;
    pair(&a, &c).await;
    // A introduced C to B, and B to C.
    wait_until(|| online(&b, &c.id) && online(&c, &b.id)).await;
    (a, b, c, dirs)
}

#[tokio::test(flavor = "multi_thread")]
async fn introductions_make_trust_transitive() {
    let (mut a, b, mut c, _dirs) = three().await;
    b.node.copy(text("from b"), vec![]).await.unwrap();
    assert_eq!(next(&mut c.clips).await.formats, text("from b"));
    assert_eq!(next(&mut a.clips).await.formats, text("from b"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unpair_spreads() {
    let (a, b, c, _dirs) = three().await;
    a.node.unpair(c.id.clone()).await.unwrap();
    wait_until(|| !paired(&a, &c.id) && !paired(&b, &c.id) && !paired(&c, &a.id)).await;
    assert!(online(&a, &b.id) && online(&b, &a.id));
    assert!(matches!(a.node.unpair(c.id.clone()).await, Err(Error::UnknownDevice)));
}

#[tokio::test(flavor = "multi_thread")]
async fn metered_devices_get_offers_for_images() {
    let (a, mut b, _da, _db) = two().await;
    b.node.set_metered(true);
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.node.copy(png(1), vec![]).await.unwrap();
    assert_eq!(next(&mut b.offers).await.mimes, vec!["image/png".to_string()]);
    a.node.copy(text("still text"), vec![]).await.unwrap();
    assert_eq!(next(&mut b.clips).await.formats, text("still text"));

    b.node.set_metered(false);
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.node.copy(png(2), vec![]).await.unwrap();
    assert_eq!(next(&mut b.clips).await.formats, png(2));
    assert!(b.offers.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn catch_up_on_reconnect() {
    let (mut a, mut b, _da, db) = two().await;
    a.node.copy(text("before"), vec![]).await.unwrap();
    next(&mut b.clips).await;

    b.node.stop().await;
    let b_id = b.id.clone();
    drop(b);
    wait_until(|| !online(&a, &b_id)).await;
    a.node.copy(text("while away"), vec![]).await.unwrap();

    // B comes back with the same data dir and connects to A on its own.
    let mut b = start(db.path(), "B").await;
    assert_eq!(b.id, b_id);
    assert_eq!(next(&mut b.clips).await.formats, text("while away"));
    assert_eq!(b.node.history().len(), 2);
    // B's newest item is older than A's, so A doesn't take it.
    quiet(&mut a.clips).await;
    quiet(&mut b.clips).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_secrets_are_rejected() {
    let dirs = [tempdir().unwrap(), tempdir().unwrap(), tempdir().unwrap()];
    let a = start(dirs[0].path(), "A").await;
    let b = start(dirs[1].path(), "B").await;
    let c = start(dirs[2].path(), "C").await;
    assert!(matches!(
        b.node.pair_link("not a link".into()).await,
        Err(Error::BadLink)
    ));

    let link = a.node.pairing_link();
    let (start_of_link, _) = link.split_once("secret=").unwrap();
    let wrong = format!("{start_of_link}secret=AAAAAAAAAAAAAAAAAAAAAAAAAA");
    for _ in 0..5 {
        assert!(matches!(
            b.node.pair_link(wrong.clone()).await,
            Err(Error::PairingRejected)
        ));
    }
    // After 5 wrong secrets, even the right one doesn't work.
    assert!(matches!(b.node.pair_link(link).await, Err(Error::PairingRejected)));
    assert!(a.node.devices().is_empty());

    // A new link works once.
    let link = a.node.pairing_link();
    b.node.pair_link(link.clone()).await.unwrap();
    assert!(matches!(c.node.pair_link(link).await, Err(Error::PairingRejected)));
    assert!(!paired(&a, &c.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn joining_a_device_that_has_many_others() {
    let (da, db) = (tempdir().unwrap(), tempdir().unwrap());
    // A has 10 devices already, each seen at 8 addresses (IPv6 privacy addresses pile up).
    let addrs: Vec<String> = (0..8)
        .map(|i| format!("[2001:db8:1234:5678:9abc:def0:1234:{i}]:47820"))
        .collect();
    let peers: Vec<_> = (0..10)
        .map(|i| {
            serde_json::json!({
                "id": iroh::SecretKey::generate().public().to_string(),
                "name": format!("Device {i}"),
                "platform": "android",
                "addrs": addrs,
            })
        })
        .collect();
    std::fs::write(da.path().join("peers.json"), serde_json::to_vec(&peers).unwrap()).unwrap();
    let a = start(da.path(), "A").await;
    let b = start(db.path(), "B").await;
    pair(&a, &b).await;
    assert_eq!(b.node.devices().len(), 11);
}

#[tokio::test(flavor = "multi_thread")]
async fn paused_sends_and_takes_nothing() {
    let (mut a, mut b, _da, _db) = two().await;
    a.node.set_paused(true);
    a.node.copy(text("not sent"), vec![]).await.unwrap();
    b.node.copy(text("dropped"), vec![]).await.unwrap();
    b.node.copy(png(3), vec![]).await.unwrap();
    quiet(&mut b.clips).await;
    quiet(&mut a.clips).await;
    assert!(a.node.history().is_empty());

    a.node.set_paused(false);
    a.node.copy(text("sent"), vec![]).await.unwrap();
    assert_eq!(next(&mut b.clips).await.formats, text("sent"));
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.local_addr().unwrap().port()
}

/// Something else takes a node's old port and drops what arrives there, like a device that changed address.
fn hold(port: u16) -> Vec<std::net::UdpSocket> {
    ["0.0.0.0", "::"]
        .into_iter()
        .filter_map(|ip| std::net::UdpSocket::bind((ip, port)).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn found_reconnects_after_restarts_on_new_ports() {
    let (a, b, da, db) = two().await;
    let old = [a.node.port(), b.node.port()];
    a.node.stop().await;
    b.node.stop().await;
    drop((a, b));
    let _held: Vec<_> = old.into_iter().flat_map(hold).collect();

    // Both come back on new ports, so each one's own dial goes to the other's old port and gets nowhere.
    let a = start_on(da.path(), "A", free_port()).await;
    let b = start_on(db.path(), "B", free_port()).await;
    b.node.found(a.id.clone(), format!("127.0.0.1:{}", a.node.port()));
    wait_until(|| online(&a, &b.id) && online(&b, &a.id)).await;
}
