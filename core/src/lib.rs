//! Shared Clipboard core: device identity, pairing, the wire protocol (`docs/protocol.md`) and transfers.
//! Every app uses it: the Linux app directly, the Apple and Android apps through UniFFI. The apps do the rest:
//! watching and writing the clipboard, finding devices (Bonjour/NSD/mDNS) and staying alive in the background.

uniffi::setup_scaffolding!();

use std::sync::Arc;

/// The wire protocol version sent in `Hello`. Not the release version (VERSIONING.md).
pub const PROTOCOL_VERSION: u32 = 1;
/// The UDP port we try first, so firewalls can open one port.
pub const DEFAULT_PORT: u16 = 47820;
/// Items bigger than this are only announced (`Events::on_offer`); `Node::fetch` gets them.
pub const SIZE_CAP: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Platform {
    Linux,
    Macos,
    Android,
    Ios,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Config {
    /// App-private folder for the key, paired devices, history and received files.
    pub data_dir: String,
    /// Shown on the other devices, e.g. "Anil's MacBook".
    pub name: String,
    pub platform: Platform,
    /// UDP port to listen on; 0 means `DEFAULT_PORT`, or any free port if that one is taken.
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Device {
    /// The device's public key, as text. Also its iroh address.
    pub id: String,
    pub name: String,
    pub platform: Platform,
    pub online: bool,
}

/// One representation of a clipboard item, e.g. `text/plain;charset=utf-8` or `image/png`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Format {
    pub mime: String,
    pub data: Vec<u8>,
}

/// A copied file. On copy, `path` is where it is; on receive, where the core saved it (inside `data_dir`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct File {
    pub name: String,
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Clip {
    /// Hash of the content: the same item has the same id on every device.
    pub id: String,
    /// Id of the device it was copied on.
    pub from: String,
    /// When it was copied, in milliseconds since 1970, by the sender's clock.
    pub time_ms: u64,
    pub formats: Vec<Format>,
    pub files: Vec<File>,
}

/// An item announced but not sent: bigger than `SIZE_CAP`, or held back on mobile data.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Offer {
    pub id: String,
    pub from: String,
    pub time_ms: u64,
    /// Total size in bytes.
    pub size: u64,
    pub mimes: Vec<String>,
    pub file_names: Vec<String>,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum Error {
    #[error("not a pairing link")]
    BadLink,
    #[error("pairing was rejected or timed out")]
    PairingRejected,
    #[error("unknown or unpaired device")]
    UnknownDevice,
    #[error("no such item")]
    NotFound,
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Network(String),
}

/// What the core tells the app. Called from the core's threads: don't block, hand the work to the UI thread.
#[uniffi::export(with_foreign)]
pub trait Events: Send + Sync {
    /// Put this item on the local clipboard. When the clipboard watcher then reports it to `Node::copy`, the core
    /// sees it's the same item and doesn't send it back.
    fn on_clip(&self, clip: Clip);
    /// An item that wasn't sent right away; show it, and call `Node::fetch` if the user wants it.
    fn on_offer(&self, offer: Offer);
    /// The paired devices changed: one paired, unpaired, came online, went offline or was renamed.
    fn on_devices(&self, devices: Vec<Device>);
    /// Pairing without a link (`Node::pair_device`): both devices show `code`, and the user confirms on each one
    /// that they match with `Node::confirm_pairing`.
    fn on_pairing_code(&self, device: Device, code: String);
}

#[derive(uniffi::Object)]
pub struct Node {}

#[uniffi::export(async_runtime = "tokio")]
impl Node {
    /// Loads or creates this device's key, starts listening and connects to paired devices it can reach.
    #[uniffi::constructor]
    pub async fn start(config: Config, events: Arc<dyn Events>) -> Result<Arc<Self>, Error> {
        let _ = (config, events);
        unimplemented!()
    }

    /// This device's id (its public key), for the discovery TXT record.
    pub fn id(&self) -> String {
        unimplemented!()
    }

    /// The UDP port it listens on, for the discovery record.
    pub fn port(&self) -> u16 {
        unimplemented!()
    }

    /// Discovery found device `id` at `addr` (e.g. "192.168.1.5:47820"). Connects if it's paired; remembers the
    /// address for `pair_device` if not.
    pub fn found(&self, id: String, addr: String) {
        let _ = (id, addr);
        unimplemented!()
    }

    /// The local clipboard changed. Sends the item to the paired devices, unless it's the one the core just gave
    /// the app (`Events::on_clip`). Leave out password-manager items before calling this; the core sends all it gets.
    pub async fn copy(&self, formats: Vec<Format>, files: Vec<File>) -> Result<(), Error> {
        let _ = (formats, files);
        unimplemented!()
    }

    /// Gets an offered item (`Events::on_offer`) from the device that has it.
    pub async fn fetch(&self, id: String) -> Result<Clip, Error> {
        let _ = id;
        unimplemented!()
    }

    /// The last 50 items, newest first.
    pub fn history(&self) -> Vec<Clip> {
        unimplemented!()
    }

    /// The paired devices.
    pub fn devices(&self) -> Vec<Device> {
        unimplemented!()
    }

    /// A link to show as a QR code: this device's id, addresses and a one-time secret, valid for 10 minutes.
    pub fn pairing_link(&self) -> String {
        unimplemented!()
    }

    /// Pairs with the device that showed `link`.
    pub async fn pair_link(&self, link: String) -> Result<Device, Error> {
        let _ = link;
        unimplemented!()
    }

    /// Pairs with a device found by discovery, comparing a 6-digit code (`Events::on_pairing_code`).
    pub async fn pair_device(&self, id: String) -> Result<Device, Error> {
        let _ = id;
        unimplemented!()
    }

    /// The user says the code shown for device `id` matches (or not).
    pub fn confirm_pairing(&self, id: String, accept: bool) {
        let _ = (id, accept);
        unimplemented!()
    }

    /// Forgets device `id`, and tells the other paired devices to forget it too.
    pub async fn unpair(&self, id: String) -> Result<(), Error> {
        let _ = id;
        unimplemented!()
    }

    /// On mobile data or a limited network: text still goes right away, everything else becomes an offer.
    pub fn set_metered(&self, metered: bool) {
        let _ = metered;
        unimplemented!()
    }

    /// Pause: nothing is sent or received until unpaused.
    pub fn set_paused(&self, paused: bool) {
        let _ = paused;
        unimplemented!()
    }

    /// Closes all connections. The node can't be used afterwards.
    pub async fn stop(&self) {
        unimplemented!()
    }
}
