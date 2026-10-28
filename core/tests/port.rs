//! Which UDP port a node takes. Its own file, so the nodes in `sync.rs` don't hold `DEFAULT_PORT` meanwhile.

use std::{path::Path, sync::Arc};

use sharedclip::{Clip, Config, DEFAULT_PORT, Device, Events, Node, Offer, Platform};
use tempfile::tempdir;

struct Nothing;

impl Events for Nothing {
    fn on_clip(&self, _: Clip) {}
    fn on_offer(&self, _: Offer) {}
    fn on_devices(&self, _: Vec<Device>) {}
    fn on_pairing_code(&self, _: Device, _: String) {}
}

async fn start(dir: &Path) -> Arc<Node> {
    let config = Config {
        data_dir: dir.to_string_lossy().into_owned(),
        name: "N".into(),
        platform: Platform::Linux,
        port: 0,
    };
    Node::start(config, Arc::new(Nothing)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn port_zero_takes_the_default_port_or_any_free_one() {
    if std::net::UdpSocket::bind(("0.0.0.0", DEFAULT_PORT)).is_err() {
        eprintln!("skipped: something else has port {DEFAULT_PORT}");
        return;
    }
    let (da, db) = (tempdir().unwrap(), tempdir().unwrap());
    let a = start(da.path()).await;
    assert_eq!(a.port(), DEFAULT_PORT);
    // A has it now, so B gets another one.
    let b = start(db.path()).await;
    assert!(b.port() != 0 && b.port() != DEFAULT_PORT);
    a.stop().await;
    b.stop().await;
}
