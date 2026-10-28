//! Shared Clipboard core: device identity, pairing, the wire protocol (`docs/protocol.md`) and transfers.
//! Every app uses it: the Linux app directly, the Apple and Android apps through UniFFI. The apps do the rest:
//! watching and writing the clipboard, finding devices (Bonjour/NSD/mDNS) and staying alive in the background.

uniffi::setup_scaffolding!();

mod bluetooth;
mod node;
mod pairing;
mod protocol;
mod store;

use std::{sync::Arc, time::Duration};

pub use bluetooth::{BluetoothLink, BluetoothSink};
use tokio::io::{AsyncRead, AsyncWrite};

/// The wire protocol version sent in `Hello`. Not the release version (VERSIONING.md).
pub const PROTOCOL_VERSION: u32 = 1;
/// The UDP port we try first, so firewalls can open one port.
pub const DEFAULT_PORT: u16 = 47820;
/// Items bigger than this are only announced (`Events::on_offer`); `Node::fetch` gets them.
pub const SIZE_CAP: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// Connected over the network or Bluetooth.
    pub online: bool,
    /// Connected only over Bluetooth: small items arrive, bigger ones and files come as offers.
    pub bluetooth: bool,
}

/// One representation of a clipboard item, e.g. `text/plain;charset=utf-8` or `image/png`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Format {
    pub mime: String,
    pub data: Vec<u8>,
}

/// A copied file. On copy, `path` is where it is; on receive, where the core saved it (inside `data_dir`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record, serde::Serialize, serde::Deserialize)]
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

/// A history item without its data; `Node::item` gets the whole item.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Summary {
    pub id: String,
    pub from: String,
    pub time_ms: u64,
    /// Its formats' MIME types.
    pub mimes: Vec<String>,
    /// Total size in bytes, formats and files.
    pub size: u64,
    /// The first 300 characters of its `text/plain` format, if it has one.
    pub text: Option<String>,
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
pub struct Node {
    inner: Arc<node::Inner>,
}

#[uniffi::export(async_runtime = "tokio")]
impl Node {
    /// Loads or creates this device's key, starts listening and connects to paired devices it can reach. Devices
    /// that are offline are dialed again now and then (5 s, doubling up to 5 minutes apart).
    /// Must be called on a tokio runtime (UniFFI provides one); the node keeps running on it.
    #[uniffi::constructor]
    pub async fn start(config: Config, events: Arc<dyn Events>) -> Result<Arc<Self>, Error> {
        Ok(Arc::new(Self {
            inner: node::Inner::start(config, events).await?,
        }))
    }

    /// This device's id (its public key), for the discovery TXT record.
    pub fn id(&self) -> String {
        self.inner.id.clone()
    }

    /// The UDP port it listens on, for the discovery record.
    pub fn port(&self) -> u16 {
        self.inner.port
    }

    /// Discovery found device `id` at `addr` (e.g. "192.168.1.5:47820"). Connects if it's paired and not connected;
    /// remembers the address for later, or for `pair_device` if it isn't paired.
    pub fn found(&self, id: String, addr: String) {
        self.inner.found(&id, &addr)
    }

    /// The local clipboard changed. Sends the item to the paired devices, unless it's one of the last 8 the core gave
    /// the app (`Events::on_clip`, `fetch`). Leave out password-manager items before calling this; the core sends
    /// all it gets. Returns once each connected device has it (or turned it down, or went away), or with
    /// `Error::Network` after 30 s plus 1 s per MB while the sends go on: don't hold up the clipboard watcher on it.
    /// Items go into the history in the order the calls start.
    pub async fn copy(&self, formats: Vec<Format>, files: Vec<File>) -> Result<(), Error> {
        self.inner.copy(formats, files).await
    }

    /// Gets an offered item (`Events::on_offer`) from the device that has it. Put the result on the clipboard
    /// yourself: it doesn't also come through `Events::on_clip`, and `copy` won't send it back.
    pub async fn fetch(&self, id: String) -> Result<Clip, Error> {
        self.inner.fetch(&id).await
    }

    /// The last 50 items, newest first, without their data.
    pub fn history(&self) -> Vec<Summary> {
        self.inner.lock().history.iter().map(|m| m.summary()).collect()
    }

    /// A whole history item, read from disk: e.g. to put an old item back on the clipboard.
    pub async fn item(&self, id: String) -> Result<Clip, Error> {
        self.inner.item(&id).await
    }

    /// The paired devices.
    pub fn devices(&self) -> Vec<Device> {
        self.inner.lock().devices()
    }

    /// A link to show as a QR code: this device's id, addresses and a one-time secret, valid for 10 minutes.
    /// A new link replaces the previous one. Call it when the pair screen opens: while a link is valid, this device
    /// also answers code pairing (`pair_device` on another device); otherwise it refuses it without asking.
    pub fn pairing_link(&self) -> String {
        pairing::link(&self.inner)
    }

    /// Pairs with the device that showed `link`.
    pub async fn pair_link(&self, link: String) -> Result<Device, Error> {
        pairing::pair_link(&self.inner, &link).await
    }

    /// Pairs with a device found by discovery, comparing a 6-digit code (`Events::on_pairing_code`). The other
    /// device must have its pair screen open (`pairing_link`).
    pub async fn pair_device(&self, id: String) -> Result<Device, Error> {
        pairing::pair_device(&self.inner, &id).await
    }

    /// The user says the code shown for device `id` matches (or not).
    pub fn confirm_pairing(&self, id: String, accept: bool) {
        pairing::confirm_pairing(&self.inner, &id, accept)
    }

    /// Forgets device `id`, and tells the other paired devices to forget it too.
    pub async fn unpair(&self, id: String) -> Result<(), Error> {
        self.inner.unpair(&id).await
    }

    /// The device's network changed (e.g. it joined another Wi-Fi): checks the connections, and dials the paired
    /// devices that are offline now instead of at their next try.
    pub async fn network_change(&self) {
        self.inner.network_change().await
    }

    /// On mobile data or a limited network: text still goes right away, everything else becomes an offer.
    pub fn set_metered(&self, metered: bool) {
        self.inner.set_metered(metered)
    }

    /// Pause: nothing is sent or received until unpaused.
    pub fn set_paused(&self, paused: bool) {
        self.inner.lock().paused = paused;
    }

    /// Runs a Bluetooth link over an L2CAP channel the app opened (`docs/protocol.md`): the core writes to the socket
    /// through `sink`, and the app passes what it reads to the returned link. The phone (central) is the `initiator`.
    /// The link is used only while the device has no network connection.
    pub fn bluetooth_open(&self, sink: Arc<dyn BluetoothSink>, initiator: bool) -> Arc<BluetoothLink> {
        self.inner.bluetooth_open(sink, initiator)
    }

    /// Whether to look for devices over Bluetooth: some paired device isn't connected over the network.
    pub fn bluetooth_wanted(&self) -> bool {
        self.inner.lock().bluetooth_wanted()
    }

    /// Closes all connections. The node can't be used afterwards.
    pub async fn stop(&self) {
        self.inner.stop().await
    }
}

impl Node {
    /// Runs a Bluetooth link on `stream` (an L2CAP channel) in the background until it closes, like `bluetooth_open`.
    pub fn bluetooth_link<S>(self: &Arc<Self>, stream: S, initiator: bool)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        self.inner.bluetooth_link(stream, initiator)
    }

    /// For tests: Bluetooth links opened afterwards close after `idle` without receiving anything (normally 150 s).
    #[doc(hidden)]
    pub fn set_bluetooth_idle(&self, idle: Duration) {
        self.inner.lock().ble_idle = idle;
    }
}

/// The Bluetooth LE service the desktops advertise.
#[uniffi::export]
pub fn bluetooth_service_uuid() -> String {
    bluetooth::SERVICE_UUID.into()
}

/// The characteristic in that service whose value is the L2CAP PSM to connect to, 2 bytes little-endian.
#[uniffi::export]
pub fn bluetooth_psm_uuid() -> String {
    bluetooth::PSM_UUID.into()
}
