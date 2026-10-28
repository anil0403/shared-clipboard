//! Helpers for the tests that run real nodes on this machine, each in its own temp dir.

#![allow(dead_code)]

use std::{path::Path, sync::Arc, time::Duration};

use sharedclip::{Clip, Config, Device, Events, Format, Node, Offer, Platform};
use tempfile::{TempDir, tempdir};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub struct Recorder {
    pub clips: UnboundedSender<Clip>,
    pub offers: UnboundedSender<Offer>,
    pub devices: UnboundedSender<Vec<Device>>,
    pub codes: UnboundedSender<(Device, String)>,
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

pub struct Dev {
    pub node: Arc<Node>,
    pub id: String,
    pub clips: UnboundedReceiver<Clip>,
    pub offers: UnboundedReceiver<Offer>,
    pub devices: UnboundedReceiver<Vec<Device>>,
    pub codes: UnboundedReceiver<(Device, String)>,
}

pub async fn start(dir: &Path, name: &str) -> Dev {
    start_on(dir, name, 0).await
}

pub async fn start_on(dir: &Path, name: &str, port: u16) -> Dev {
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
    let events = Arc::new(events);
    let config = || Config {
        data_dir: dir.to_string_lossy().into_owned(),
        name: name.into(),
        platform: Platform::Linux,
        port,
    };
    // A node stopped a moment ago can still hold its port (its last tasks drop the sockets), so wait for it.
    let mut tries = 0;
    let node = loop {
        match Node::start(config(), events.clone()).await {
            Ok(node) => break node,
            Err(e) if port != 0 && tries < 50 => {
                tries += 1;
                eprintln!("port {port} not free yet: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
    Dev {
        id: node.id(),
        node,
        clips,
        offers,
        devices,
        codes,
    }
}

pub async fn within<T>(what: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), what)
        .await
        .expect("timed out")
}

pub async fn next<T>(rx: &mut UnboundedReceiver<T>) -> T {
    within(rx.recv()).await.unwrap()
}

pub async fn wait_until(mut ok: impl FnMut() -> bool) {
    within(async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
}

/// Nothing more arrives for a moment.
pub async fn quiet<T: std::fmt::Debug>(rx: &mut UnboundedReceiver<T>) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    if let Ok(got) = rx.try_recv() {
        panic!("didn't expect {got:?}");
    }
}

pub fn online(dev: &Dev, id: &str) -> bool {
    dev.node.devices().iter().any(|d| d.id == id && d.online)
}

pub fn paired(dev: &Dev, id: &str) -> bool {
    dev.node.devices().iter().any(|d| d.id == id)
}

pub fn text(s: &str) -> Vec<Format> {
    vec![Format {
        mime: "text/plain;charset=utf-8".into(),
        data: s.as_bytes().to_vec(),
    }]
}

pub fn png(byte: u8) -> Vec<Format> {
    vec![Format {
        mime: "image/png".into(),
        data: vec![byte; 2000],
    }]
}

/// `joiner` scans `host`'s link; waits until they're connected.
pub async fn pair(host: &Dev, joiner: &Dev) {
    let device = joiner.node.pair_link(host.node.pairing_link()).await.unwrap();
    assert_eq!(device.id, host.id);
    wait_until(|| online(host, &joiner.id) && online(joiner, &host.id)).await;
}

pub async fn two() -> (Dev, Dev, TempDir, TempDir) {
    let (da, db) = (tempdir().unwrap(), tempdir().unwrap());
    let a = start(da.path(), "A").await;
    let b = start(db.path(), "B").await;
    pair(&a, &b).await;
    (a, b, da, db)
}
