//! Bluetooth LE links (`docs/protocol.md`): Noise over a reliable byte stream (an L2CAP channel), carrying the same
//! messages as the network, for paired devices that can't reach each other over it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use iroh::{PublicKey, SecretKey};
use snow::StatelessTransportState;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot},
};

use crate::{
    Error,
    node::{Inner, Via},
    protocol::{MAX_HEADER, Msg, net, read_frame},
};

pub const SERVICE_UUID: &str = "556874fd-9b25-4150-bb6d-47e1ba6fe50c";
pub const PSM_UUID: &str = "940c7880-bb22-4995-8a3f-f76155b49830";
const NOISE: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// The longest Noise message, and the most data one carries (the rest is its tag).
const MAX_NOISE: usize = 65535;
const MAX_PLAIN: usize = MAX_NOISE - 16;
/// Bigger items, and items with files, go over Bluetooth as offers.
pub const BLE_CAP: u64 = 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// A link that received nothing for this long is closed. Each side sends `hello` after 2/5 of it (60 s) without
/// sending anything else, so a quiet link stays open.
pub const IDLE: Duration = Duration::from_secs(150);

/// Numbers each link, to tell it from a link that replaced it.
static LINKS: AtomicU64 = AtomicU64::new(0);

/// Queues messages on a Bluetooth link; they go out in order.
#[derive(Clone)]
pub struct Ble {
    pub link: u64,
    tx: mpsc::UnboundedSender<Out>,
}

enum Out {
    /// A whole message, and who to tell once it's written.
    Frame(Vec<u8>, oneshot::Sender<()>),
    Close,
}

/// A Bluetooth link in `State`.
pub struct BleConn {
    pub ble: Ble,
    /// What the device's `hello` said about a metered network; `None` until it arrives.
    pub metered: Option<bool>,
}

impl Ble {
    /// Sends a message with its body (`[u32 header length][header][body]`). Returns once it's written to the link.
    pub async fn send(&self, frame: Vec<u8>) -> Result<(), Error> {
        let (done, written) = oneshot::channel();
        self.tx
            .send(Out::Frame(frame, done))
            .map_err(|_| net("the Bluetooth link closed"))?;
        written.await.map_err(|_| net("the Bluetooth link closed"))
    }

    pub async fn send_msg(&self, msg: &Msg) -> Result<(), Error> {
        self.send(frame(msg)).await
    }

    /// Closes the link once what's queued is written.
    pub fn close(&self) {
        let _ = self.tx.send(Out::Close);
    }

    pub async fn closed(&self) {
        self.tx.closed().await
    }
}

/// A message without a body.
fn frame(msg: &Msg) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("messages always serialize");
    [&(json.len() as u32).to_be_bytes()[..], &json].concat()
}

/// How long sending or receiving `size` bytes may take: 30 s plus 60 s per MiB.
pub fn ble_limit(size: u64) -> Duration {
    Duration::from_secs(30 + 60 * size / BLE_CAP)
}

/// Our Noise key: the X25519 form of our Ed25519 key (the first half of SHA-512 of its seed, clamped).
fn x25519_secret(key: &SecretKey) -> [u8; 32] {
    let mut k = key.as_signing_key().to_scalar_bytes();
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    k
}

/// A device's Noise key: its Ed25519 id as a Montgomery point.
fn x25519_public(id: &PublicKey) -> [u8; 32] {
    id.as_verifying_key().to_montgomery().to_bytes()
}

impl Inner {
    /// Runs a Bluetooth link on `stream` in the background until it closes. The central (the phone) is the
    /// `initiator`.
    pub fn bluetooth_link<S>(self: &Arc<Self>, stream: S, initiator: bool)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let this = self.clone();
        self.spawn(async move {
            let _ = this.run_link(stream, initiator).await;
        });
    }

    async fn run_link(
        self: Arc<Self>,
        mut stream: impl AsyncRead + AsyncWrite + Unpin,
        initiator: bool,
    ) -> Result<(), Error> {
        let secret = x25519_secret(self.ep.secret_key());
        let hs = handshake(&mut stream, &secret, initiator);
        let (noise, remote) = tokio::time::timeout(HANDSHAKE_TIMEOUT, hs).await.map_err(net)??;
        let (tx, rx) = mpsc::unbounded_channel();
        let ble = Ble {
            link: LINKS.fetch_add(1, Ordering::Relaxed),
            tx,
        };
        let _ = ble.tx.send(Out::Frame(frame(&self.hello()), oneshot::channel().0));
        // The link is to the paired device with that key. Removed devices aren't paired any more.
        let (id, idle, old) = {
            let mut s = self.lock();
            let mut ids = s.peers.keys();
            let id = ids
                .find(|id| id.parse().is_ok_and(|k| x25519_public(&k) == remote))
                .cloned();
            let Some(id) = id.filter(|_| !s.stopped) else {
                return Err(Error::UnknownDevice);
            };
            let conn = BleConn {
                ble: ble.clone(),
                metered: None,
            };
            let old = s.ble.insert(id.clone(), conn);
            (id, s.ble_idle, old)
        };
        // A newer link to the same device replaces the old one.
        if let Some(old) = old {
            old.ble.close();
        }
        self.emit_devices();
        let (mut r, mut w) = tokio::io::split(stream);
        let keepalive = idle * 2 / 5;
        let res = tokio::select! {
            res = write_link(&mut w, &noise, rx, keepalive, || self.hello()) => res,
            res = self.read_link(&mut r, &noise, &id, &ble, idle) => res,
        };
        let ours = {
            let mut s = self.lock();
            let ours = s.ble.get(&id).is_some_and(|b| b.ble.link == ble.link);
            if ours {
                s.ble.remove(&id);
            }
            ours
        };
        if ours {
            self.emit_devices();
        }
        res
    }

    /// Decrypts what arrives and handles each message in turn, until the link fails or is idle for `idle`.
    async fn read_link(
        self: &Arc<Self>,
        r: &mut (impl AsyncRead + Unpin),
        noise: &StatelessTransportState,
        id: &str,
        ble: &Ble,
        idle: Duration,
    ) -> Result<(), Error> {
        let (mut plain_tx, mut plain) = tokio::io::duplex(MAX_NOISE);
        let decrypt = async move {
            let mut buf = vec![0; MAX_NOISE];
            for nonce in 0.. {
                let msg = tokio::time::timeout(idle, read_noise(r)).await.map_err(net)??;
                let n = noise.read_message(nonce, &msg, &mut buf).map_err(net)?;
                plain_tx.write_all(&buf[..n]).await.map_err(net)?;
            }
            Ok(())
        };
        let handle = async {
            let via = Via::Ble(ble.clone());
            loop {
                let msg = read_frame(&mut plain, MAX_HEADER).await?;
                // Only a `clip` has a body: its formats, at most 1 MiB in all. No files.
                let len = match &msg {
                    Msg::Clip(m) if m.files.is_empty() => m
                        .formats
                        .iter()
                        .try_fold(0u64, |n, f| n.checked_add(f.size))
                        .filter(|n| *n <= BLE_CAP)
                        .ok_or_else(|| net("item too big for Bluetooth"))?,
                    Msg::Clip(_) => return Err(net("no files over Bluetooth")),
                    _ => 0,
                };
                let mut body = (&mut plain).take(len);
                if tokio::time::timeout(ble_limit(len), self.handle(id, &via, msg, &mut body))
                    .await
                    .is_err()
                {
                    return Err(net("timed out"));
                }
                // Skip what wasn't read, e.g. an item we didn't take.
                tokio::io::copy(&mut body, &mut tokio::io::sink()).await.map_err(net)?;
            }
        };
        tokio::pin!(handle);
        tokio::select! {
            res = &mut handle => res,
            // The link ended: what arrived before still counts (e.g. an `unpair` sent just before closing).
            res = decrypt => {
                let _ = handle.await;
                res
            }
        }
    }
}

/// Runs the Noise handshake. Returns the transport and the other side's static key.
async fn handshake(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    secret: &[u8; 32],
    initiator: bool,
) -> Result<(StatelessTransportState, [u8; 32]), Error> {
    let builder = snow::Builder::new(NOISE.parse().map_err(net)?)
        .local_private_key(secret)
        .map_err(net)?;
    let mut hs = if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(net)?;
    let mut buf = vec![0; MAX_NOISE];
    while !hs.is_handshake_finished() {
        if hs.is_my_turn() {
            let n = hs.write_message(&[], &mut buf).map_err(net)?;
            write_noise(stream, &buf[..n]).await?;
        } else {
            hs.read_message(&read_noise(stream).await?, &mut buf).map_err(net)?;
        }
    }
    let remote = hs.get_remote_static().and_then(|k| k.try_into().ok());
    let remote = remote.ok_or_else(|| net("no static key"))?;
    Ok((hs.into_stateless_transport_mode().map_err(net)?, remote))
}

/// Writes queued messages, each split into Noise messages. Sends `hello` when it sent nothing for `keepalive`.
async fn write_link(
    w: &mut (impl AsyncWrite + Unpin),
    noise: &StatelessTransportState,
    mut rx: mpsc::UnboundedReceiver<Out>,
    keepalive: Duration,
    hello: impl Fn() -> Msg,
) -> Result<(), Error> {
    let mut buf = vec![0; MAX_NOISE];
    let mut nonce = 0;
    loop {
        let out = tokio::time::timeout(keepalive, rx.recv())
            .await
            .unwrap_or_else(|_| Some(Out::Frame(frame(&hello()), oneshot::channel().0)));
        let Some(Out::Frame(data, done)) = out else {
            return Ok(());
        };
        let write = async {
            for chunk in data.chunks(MAX_PLAIN) {
                let n = noise.write_message(nonce, chunk, &mut buf).map_err(net)?;
                nonce += 1;
                write_noise(w, &buf[..n]).await?;
            }
            Ok::<_, Error>(())
        };
        tokio::time::timeout(ble_limit(data.len() as u64), write)
            .await
            .map_err(net)??;
        let _ = done.send(());
    }
}

/// Reads `[u16 big-endian length][Noise message]`.
async fn read_noise(r: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, Error> {
    let mut msg = vec![0; r.read_u16().await.map_err(net)? as usize];
    r.read_exact(&mut msg).await.map_err(net)?;
    Ok(msg)
}

async fn write_noise(w: &mut (impl AsyncWrite + Unpin), msg: &[u8]) -> Result<(), Error> {
    w.write_u16(msg.len() as u16).await.map_err(net)?;
    w.write_all(msg).await.map_err(net)?;
    w.flush().await.map_err(net)
}

/// Writes a Bluetooth link's bytes to the app's socket.
#[uniffi::export(with_foreign)]
pub trait BluetoothSink: Send + Sync {
    /// Write `data` to the socket. Calls come in order and never at the same time; it may block.
    fn send(&self, data: Vec<u8>);
    /// Close the socket: the link is over. Also called after `BluetoothLink::closed`.
    fn close_socket(&self);
}

/// A Bluetooth link the core runs (`Node::bluetooth_open`). The app gives it what it reads from the socket.
#[derive(uniffi::Object)]
pub struct BluetoothLink {
    tx: mpsc::UnboundedSender<Option<Vec<u8>>>,
}

#[uniffi::export]
impl BluetoothLink {
    /// Bytes read from the socket, in order, in any chunks.
    pub fn receive(&self, data: Vec<u8>) {
        let _ = self.tx.send(Some(data));
    }

    /// The socket closed or failed.
    pub fn closed(&self) {
        let _ = self.tx.send(None);
    }
}

impl Inner {
    /// Runs a Bluetooth link over the app's socket: a byte pipe between `sink`/`BluetoothLink` and `bluetooth_link`.
    pub fn bluetooth_open(self: &Arc<Self>, sink: Arc<dyn BluetoothSink>, initiator: bool) -> Arc<BluetoothLink> {
        let (link, app) = tokio::io::duplex(MAX_NOISE);
        let (mut from_link, mut to_link) = tokio::io::split(app);
        let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
        self.bluetooth_link(link, initiator);
        self.spawn(async move {
            while let Some(Some(data)) = rx.recv().await {
                if to_link.write_all(&data).await.is_err() {
                    break;
                }
            }
            let _ = to_link.shutdown().await;
        });
        // One task, so `send` is never called twice at once; off the async threads, as it may block.
        self.spawn(async move {
            let mut buf = vec![0; MAX_NOISE];
            while let Ok(n @ 1..) = from_link.read(&mut buf).await {
                let (sink, data) = (sink.clone(), buf[..n].to_vec());
                if tokio::task::spawn_blocking(move || sink.send(data)).await.is_err() {
                    break;
                }
            }
            let _ = tokio::task::spawn_blocking(move || sink.close_socket()).await;
        });
        Arc::new(BluetoothLink { tx })
    }
}
