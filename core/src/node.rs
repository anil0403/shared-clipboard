//! The running node: one connection to each paired device that's online, and the messages on it.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use iroh::{
    Endpoint, EndpointAddr, PublicKey, RelayMode, SecretKey, TransportAddr,
    endpoint::{
        BindOpts, Connection, ConnectionError, IdleTimeout, IncomingAddr, QuicTransportConfig, RecvStream, Side,
        VarInt, presets,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
};

use crate::{
    Clip, Config, DEFAULT_PORT, Device, Error, Events, File, Format, Offer, PROTOCOL_VERSION, Platform, SIZE_CAP,
    pairing::{self, Link},
    protocol::{
        ALPN, BODY_SLACK, ClipMsg, FilePart, FormatPart, MAX_FILE_NAME, MAX_HEADER, MAX_NAME, Msg, OfferMsg, PAIR_ALPN,
        PeerInfo, clean, hash_part, net, read_frame, send_msg, valid_id, write_frame,
    },
    store::{self, HISTORY_LEN, Meta, PREVIEW_BYTES, io},
};

const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// How many addresses we keep for each device, newest first.
const MAX_ADDRS: usize = 8;
/// How many offers we remember for `fetch`.
const MAX_OFFERS: usize = 50;
/// How many items given to the app we remember, so the app reporting them back doesn't send them again.
const MAX_DELIVERED: usize = 8;

// Why we closed a connection.
const CLOSE_STOP: u32 = 0;
const CLOSE_DUPLICATE: u32 = 1;
const CLOSE_NOT_PAIRED: u32 = 2;
const CLOSE_UNPAIRED: u32 = 3;

pub struct Inner {
    pub ep: Endpoint,
    /// Our id, as text.
    pub id: String,
    pub name: String,
    pub platform: Platform,
    pub port: u16,
    pub events: Arc<dyn Events>,
    dir: PathBuf,
    /// The runtime `start` ran on; sync methods spawn their work here.
    rt: tokio::runtime::Handle,
    state: Mutex<State>,
    /// Held while writing the state's files, so the last write has the newest state.
    saving: Mutex<()>,
}

#[derive(Default)]
pub struct State {
    /// Paired devices, by id.
    pub peers: BTreeMap<String, PeerInfo>,
    /// The connection to each paired device that's online.
    conns: HashMap<String, Conn>,
    /// Addresses of unpaired devices that discovery found, for `pair_device`.
    pub found: HashMap<String, Vec<String>>,
    dialing: HashSet<String>,
    /// Newest first. Only what the list shows; the data is on disk (`store::load_item`).
    pub history: Vec<Meta>,
    /// The last items given to the app, newest first; when the app reports one back, it isn't sent again. More than
    /// one, because the clipboard watcher may report an item after the next one arrived.
    delivered: VecDeque<String>,
    /// Offers we got, newest first: (item id, device that has it, size).
    offers: VecDeque<(String, String, u64)>,
    /// `fetch` calls waiting for an item to be in the history.
    fetches: HashMap<String, Vec<oneshot::Sender<Result<(), Error>>>>,
    metered: bool,
    pub paused: bool,
    pub link: Option<Link>,
    /// A pairing is running.
    pub pairing: bool,
    /// The user's answer to a pairing code, by device id.
    pub confirms: HashMap<String, oneshot::Sender<bool>>,
    stopped: bool,
    /// Devices the user unpaired: another device listing them (`peers`, `paired`) doesn't bring them back.
    removed: HashSet<String>,
}

struct Conn {
    conn: Connection,
    /// Whether the device is on a metered network; `None` until its `hello` arrives.
    metered: Option<bool>,
}

impl State {
    pub fn devices(&self) -> Vec<Device> {
        let device = |p: &PeerInfo| Device {
            id: p.id.clone(),
            name: p.name.clone(),
            platform: p.platform,
            online: self.conns.contains_key(&p.id),
        };
        self.peers.values().map(device).collect()
    }

    /// Puts an item first in the history. Its data must already be saved. Returns the ids that dropped out, for
    /// `Inner::save_history`.
    fn add_history(&mut self, meta: Meta) -> Vec<String> {
        self.history.retain(|h| h.id != meta.id);
        self.history.insert(0, meta);
        let keep = self.history.len().min(HISTORY_LEN);
        self.history.split_off(keep).into_iter().map(|h| h.id).collect()
    }

    /// A catch-up (`latest`) item counts only if it's newer than the newest one we have.
    fn is_newer(&self, time_ms: u64) -> bool {
        self.history.first().is_none_or(|c| time_ms > c.time_ms)
    }
}

impl Inner {
    pub async fn start(config: Config, events: Arc<dyn Events>) -> Result<Arc<Self>, Error> {
        let rt = tokio::runtime::Handle::try_current().map_err(|_| io("start must run on a tokio runtime"))?;
        let dir = PathBuf::from(&config.data_dir);
        std::fs::create_dir_all(&dir).map_err(io)?;
        // Only this user may read the key, paired devices and history. Best effort: an iOS App Group container may
        // not allow it, and the sandbox guards it there anyway.
        #[cfg(unix)]
        let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let key = store::load_key(&dir)?;
        let ep = match bind(key.clone(), if config.port == 0 { DEFAULT_PORT } else { config.port }).await {
            Err(_) if config.port == 0 => bind(key, 0).await?,
            ep => ep?,
        };
        let port = ep.bound_sockets().iter().find(|a| a.is_ipv4()).map_or(0, |a| a.port());
        let state = State {
            peers: store::load_peers(&dir).into_iter().map(|p| (p.id.clone(), p)).collect(),
            removed: store::load_removed(&dir),
            history: store::load_history(&dir),
            ..Default::default()
        };
        let inner = Arc::new(Inner {
            id: ep.id().to_string(),
            ep,
            name: config.name,
            platform: config.platform,
            port,
            events,
            dir,
            rt,
            state: Mutex::new(state),
            saving: Mutex::new(()),
        });
        inner.spawn(inner.clone().accept_loop());
        let ids: Vec<String> = inner.lock().peers.keys().cloned().collect();
        for id in ids {
            inner.connect(&id);
        }
        Ok(inner)
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.rt.spawn(task);
    }

    /// Saves the paired and unpaired devices. Call it without the state lock.
    fn save_peers(&self) {
        let _saving = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        let (peers, removed) = {
            let s = self.lock();
            (s.peers.values().cloned().collect::<Vec<_>>(), s.removed.clone())
        };
        store::save_peers(&self.dir, &peers, &removed);
    }

    pub fn emit_devices(&self) {
        let devices = self.lock().devices();
        self.events.on_devices(devices);
    }

    async fn accept_loop(self: Arc<Self>) {
        while let Some(incoming) = self.ep.accept().await {
            let addr = match incoming.remote_addr() {
                IncomingAddr::Ip(addr) => Some(addr),
                _ => None,
            };
            let this = self.clone();
            self.spawn(async move {
                let Ok(conn) = incoming.await else { return };
                if conn.alpn() == PAIR_ALPN {
                    pairing::respond(this, conn, addr).await;
                } else {
                    this.add_conn(conn, addr);
                }
            });
        }
    }

    /// Dials paired device `id` at the addresses we know, unless a dial is already running. That one dials again
    /// if it fails and the addresses changed meanwhile.
    pub fn connect(self: &Arc<Self>, id: &str) {
        let Ok(key) = id.parse::<PublicKey>() else { return };
        let addrs = {
            let mut s = self.lock();
            let addrs = s.peers.get(id).map(|p| parse_addrs(&p.addrs)).unwrap_or_default();
            if s.stopped || addrs.is_empty() || !s.dialing.insert(id.to_string()) {
                return;
            }
            addrs
        };
        let this = self.clone();
        let id = id.to_string();
        self.spawn(async move {
            let addr = EndpointAddr::from_parts(key, addrs.iter().copied().map(TransportAddr::Ip));
            let conn = tokio::time::timeout(DIAL_TIMEOUT, this.ep.connect(addr, ALPN)).await;
            // An address found while this dial ran wasn't tried (the dial was running): try it now.
            let moved = {
                let mut s = this.lock();
                s.dialing.remove(&id);
                !s.conns.contains_key(&id) && s.peers.get(&id).is_some_and(|p| parse_addrs(&p.addrs) != addrs)
            };
            match conn {
                Ok(Ok(conn)) => this.add_conn(conn, None),
                _ if moved => this.connect(&id),
                _ => {}
            }
        });
    }

    /// A `sharedclip/1` connection is up. Keeps it if the device is paired, applying the duplicate rule.
    fn add_conn(self: &Arc<Self>, conn: Connection, addr: Option<SocketAddr>) {
        let id = conn.remote_id().to_string();
        let opener = |c: &Connection| {
            if c.side() == Side::Client {
                self.id.clone()
            } else {
                id.clone()
            }
        };
        let (came_online, moved) = {
            let mut s = self.lock();
            let s = &mut *s;
            let Some(peer) = s.peers.get_mut(&id).filter(|_| !s.stopped) else {
                conn.close(VarInt::from_u32(CLOSE_NOT_PAIRED), b"not paired");
                return;
            };
            let moved = addr.is_some_and(|a| remember(&mut peer.addrs, a));
            // Two connections: keep the one opened by the lower id (or the newer, if one device opened both).
            let old = s
                .conns
                .get(&id)
                .map(|c| c.conn.clone())
                .filter(|c| c.close_reason().is_none());
            match &old {
                Some(old) if opener(old) < opener(&conn) => {
                    conn.close(VarInt::from_u32(CLOSE_DUPLICATE), b"duplicate");
                    (None, moved)
                }
                _ => {
                    if let Some(old) = &old {
                        old.close(VarInt::from_u32(CLOSE_DUPLICATE), b"duplicate");
                    }
                    let c = Conn {
                        conn: conn.clone(),
                        metered: None,
                    };
                    s.conns.insert(id.clone(), c);
                    (Some(old.is_none()), moved)
                }
            }
        };
        if moved {
            self.save_peers();
        }
        let Some(came_online) = came_online else { return };
        if came_online {
            self.emit_devices();
        }
        let this = self.clone();
        self.spawn(async move {
            let _ = send_msg(&conn, &this.hello()).await;
            this.read_loop(id, conn).await;
        });
    }

    fn hello(&self) -> Msg {
        let metered = self.lock().metered;
        Msg::Hello {
            name: self.name.clone(),
            platform: self.platform,
            protocol: PROTOCOL_VERSION,
            features: if metered { vec!["metered".into()] } else { vec![] },
        }
    }

    async fn read_loop(self: Arc<Self>, id: String, conn: Connection) {
        while let Ok(recv) = conn.accept_uni().await {
            let (this, id, cid) = (self.clone(), id.clone(), conn.stable_id());
            self.spawn(async move {
                let _ = this.handle(&id, cid, recv).await;
            });
        }
        let retry = {
            let mut s = self.lock();
            if s.conns.get(&id).is_none_or(|c| c.conn.stable_id() != conn.stable_id()) {
                return;
            }
            s.conns.remove(&id);
            matches!(conn.close_reason(), Some(ConnectionError::TimedOut))
        };
        self.emit_devices();
        // The network dropped: try once more; after that, discovery (`found`) brings it back.
        if retry {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !self.lock().conns.contains_key(&id) {
                self.connect(&id);
            }
        }
    }

    /// One incoming message. `cid` is the connection it came on.
    async fn handle(self: &Arc<Self>, from: &str, cid: usize, mut recv: RecvStream) -> Result<(), Error> {
        match read_frame(&mut recv, MAX_HEADER).await? {
            Msg::Hello {
                name,
                platform,
                features,
                ..
            } => self.on_hello(
                from,
                cid,
                clean(&name, MAX_NAME),
                platform,
                features.iter().any(|f| f == "metered"),
            ),
            Msg::Clip(m) => self.on_clip(from, recv, m).await?,
            Msg::Offer(m) => self.on_offer(from, m),
            Msg::Request { id } => self.on_request(from, &id),
            Msg::Missing { id } => self.on_missing(from, &id),
            Msg::Peers { devices } => self.on_peers(devices),
            Msg::Unpair { id } => {
                // `id` is another device to forget, or the sender itself: it forgot us.
                if let Ok(key) = id.parse::<PublicKey>()
                    && key.to_string() != self.id
                {
                    self.forget(&key.to_string());
                }
            }
            Msg::Unknown => {}
        }
        Ok(())
    }

    fn on_hello(self: &Arc<Self>, from: &str, cid: usize, name: String, platform: Platform, metered: bool) {
        let (renamed, first) = {
            let mut s = self.lock();
            let s = &mut *s;
            let mut renamed = false;
            if let Some(p) = s.peers.get_mut(from)
                && (p.name != name || p.platform != platform)
            {
                (p.name, p.platform, renamed) = (name, platform, true);
            }
            let first = match s.conns.get_mut(from) {
                Some(c) if c.conn.stable_id() == cid => c.metered.replace(metered).is_none(),
                _ => false,
            };
            (renamed, first)
        };
        if renamed {
            self.save_peers();
            self.emit_devices();
        }
        // Catch-up: once we know what the device accepts, send it our newest item.
        if first {
            let latest = {
                let s = self.lock();
                let latest = s.history.first().zip(s.conns.get(from)).filter(|_| !s.paused);
                latest.map(|(meta, c)| (meta.clone(), c.conn.clone()))
            };
            if let Some((meta, conn)) = latest {
                let this = self.clone();
                self.spawn(async move {
                    let _ = send_item(&conn, &this.dir, &meta, metered, true).await;
                });
            }
        }
    }

    async fn on_clip(self: &Arc<Self>, from: &str, recv: RecvStream, m: ClipMsg) -> Result<(), Error> {
        let id = m.id.clone();
        let res = self.receive_clip(from, recv, m).await;
        if res.is_err() {
            // A `fetch` waiting for it fails now, not at its timeout.
            self.lock().fetches.remove(&id);
        }
        res
    }

    async fn receive_clip(self: &Arc<Self>, from: &str, mut recv: RecvStream, m: ClipMsg) -> Result<(), Error> {
        if !valid_id(&m.id) {
            return Err(net("bad item id"));
        }
        let mut sizes = m.formats.iter().map(|f| f.size).chain(m.files.iter().map(|f| f.size));
        let total = sizes.try_fold(0u64, u64::checked_add).ok_or_else(|| net("bad size"))?;
        {
            let s = self.lock();
            // An item we asked for may be as big as its offer said; anything else must fit under the cap.
            let fetching = s.fetches.contains_key(&m.id);
            let cap = s
                .offers
                .iter()
                .find(|o| o.0 == m.id && fetching)
                .map_or(SIZE_CAP, |o| o.2.max(SIZE_CAP));
            if total > cap.saturating_add(BODY_SLACK) {
                return Err(net("item too big"));
            }
            if !fetching && (s.paused || (m.latest && !s.is_newer(m.time_ms))) {
                return Ok(());
            }
        }

        // Formats and files arrive in part files of their own and move in place once they match the id, so a transfer
        // that fails, or runs at the same time, never touches the data of the same item that the history points to.
        let mut h = blake3::Hasher::new();
        let blob_part = store::blob_part(&self.dir, &m.id).await?;
        let dir = self.dir.join("files").join(&m.id);
        let part = self
            .dir
            .join("files")
            .join(format!("{}.{:x}", m.id, rand::random::<u64>()));
        let got = async {
            let text = receive_formats(&mut recv, &mut h, &blob_part, &m.formats).await?;
            let files = receive_files(&mut recv, &mut h, &part, &dir, m.files).await?;
            Ok::<_, Error>((text, files))
        };
        let (text, files) = match got.await {
            Ok(got) if h.finalize().to_hex().as_str() == m.id => got,
            _ => {
                let _ = tokio::fs::remove_file(&blob_part).await;
                let _ = tokio::fs::remove_dir_all(&part).await;
                return Err(net("the item didn't match its id"));
            }
        };
        if !files.is_empty() {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            // If another transfer of this item got there first, keep its files: they're the same.
            if tokio::fs::rename(&part, &dir).await.is_err() {
                let _ = tokio::fs::remove_dir_all(&part).await;
            }
        }
        tokio::fs::rename(&blob_part, store::blob_path(&self.dir, &m.id))
            .await
            .map_err(io)?;
        let meta = Meta {
            id: m.id,
            from: from.to_string(),
            time_ms: m.time_ms,
            formats: m.formats,
            files,
            text,
        };

        let (waiters, forgotten) = {
            let mut s = self.lock();
            let waiters = s.fetches.remove(&meta.id);
            if waiters.is_none() && (s.paused || (m.latest && !s.is_newer(meta.time_ms))) {
                // Paused, or something newer came meanwhile: drop what we saved, unless the history has it.
                let keep = s.history.iter().any(|h| h.id == meta.id);
                drop(s);
                if !keep {
                    self.save_history(vec![meta.id]);
                }
                return Ok(());
            }
            s.delivered.retain(|d| *d != meta.id);
            s.delivered.push_front(meta.id.clone());
            s.delivered.truncate(MAX_DELIVERED);
            let forgotten = s.add_history(meta.clone());
            (waiters, forgotten)
        };
        self.save_history(forgotten);
        match waiters {
            // Each `fetch` reads it from disk itself.
            Some(waiters) => waiters.into_iter().for_each(|w| _ = w.send(Ok(()))),
            None => self.events.on_clip(store::load_item(&self.dir, &meta).await?),
        }
        Ok(())
    }

    fn on_offer(&self, from: &str, m: OfferMsg) {
        {
            let mut s = self.lock();
            let have = s.history.iter().any(|h| h.id == m.id);
            if s.paused || have || !valid_id(&m.id) || (m.latest && !s.is_newer(m.time_ms)) {
                return;
            }
            s.offers.retain(|o| o.0 != m.id);
            s.offers.push_front((m.id.clone(), from.to_string(), m.size));
            s.offers.truncate(MAX_OFFERS);
        }
        let offer = Offer {
            id: m.id,
            from: from.to_string(),
            time_ms: m.time_ms,
            size: m.size,
            mimes: m.mimes,
            file_names: m.file_names.iter().map(|n| clean(n, MAX_FILE_NAME)).collect(),
        };
        self.events.on_offer(offer);
    }

    /// Sends the item asked for, or `missing` if we don't have it or are paused.
    fn on_request(&self, from: &str, id: &str) {
        let (meta, conn) = {
            let s = self.lock();
            let meta = s.history.iter().find(|h| h.id == id).filter(|_| !s.paused).cloned();
            let Some(conn) = s.conns.get(from).map(|c| c.conn.clone()) else {
                return;
            };
            (meta, conn)
        };
        let (dir, id) = (self.dir.clone(), id.to_string());
        self.spawn(async move {
            let _ = match meta {
                Some(meta) => send_clip(&conn, &dir, &meta, false).await,
                None => send_msg(&conn, &Msg::Missing { id }).await,
            };
        });
    }

    /// The device we asked for an item doesn't have it: its `fetch` calls fail now.
    fn on_missing(&self, from: &str, id: &str) {
        let mut s = self.lock();
        if s.offers.iter().any(|o| o.0 == id && o.1 == from)
            && let Some(waiters) = s.fetches.remove(id)
        {
            waiters.into_iter().for_each(|w| _ = w.send(Err(Error::NotFound)));
        }
    }

    fn on_peers(self: &Arc<Self>, devices: Vec<PeerInfo>) {
        let added = self.add_peers(devices);
        if !added.is_empty() {
            self.emit_devices();
        }
        for id in added {
            self.connect(&id);
        }
    }

    /// The user paired with this device directly: trust it, even if it was unpaired before.
    pub fn add_paired(&self, peer: PeerInfo) {
        self.lock().removed.remove(&peer.id);
        self.add_peers(vec![peer]);
    }

    /// Trusts these devices, except ones the user unpaired. Known ones only get their addresses updated. Returns the
    /// ids that are new.
    pub fn add_peers(&self, devices: Vec<PeerInfo>) -> Vec<String> {
        let mut added = Vec::new();
        let mut s = self.lock();
        for mut d in devices {
            let Ok(key) = d.id.parse::<PublicKey>() else { continue };
            d.id = key.to_string();
            if d.id == self.id || s.removed.contains(&d.id) {
                continue;
            }
            d.name = clean(&d.name, MAX_NAME);
            let addrs = parse_addrs(&d.addrs);
            match s.peers.get_mut(&d.id) {
                Some(p) => addrs.into_iter().rev().for_each(|a| _ = remember(&mut p.addrs, a)),
                None => {
                    d.addrs = addrs.iter().take(MAX_ADDRS).map(|a| a.to_string()).collect();
                    added.push(d.id.clone());
                    s.peers.insert(d.id.clone(), d);
                }
            }
        }
        drop(s);
        self.save_peers();
        added
    }

    /// Tells the other connected devices about a newly paired one, so they trust it too.
    pub fn introduce(&self, peer: &PeerInfo) {
        let conns: Vec<Connection> = self
            .lock()
            .conns
            .iter()
            .filter(|(id, _)| **id != peer.id)
            .map(|(_, c)| c.conn.clone())
            .collect();
        for conn in conns {
            let msg = Msg::Peers {
                devices: vec![peer.clone()],
            };
            self.spawn(async move {
                let _ = send_msg(&conn, &msg).await;
            });
        }
    }

    /// Forgets a device and closes its connection.
    fn forget(&self, id: &str) {
        let conn = {
            let mut s = self.lock();
            if s.peers.remove(id).is_none() {
                return;
            }
            s.removed.insert(id.to_string());
            s.conns.remove(id)
        };
        self.save_peers();
        if let Some(c) = conn {
            c.conn.close(VarInt::from_u32(CLOSE_UNPAIRED), b"unpaired");
        }
        self.emit_devices();
    }

    pub fn found(self: &Arc<Self>, id: &str, addr: &str) {
        let (Ok(key), Ok(addr)) = (id.parse::<PublicKey>(), addr.parse::<SocketAddr>()) else {
            return;
        };
        let id = key.to_string();
        if id == self.id {
            return;
        }
        let (new, dial) = {
            let mut s = self.lock();
            let s = &mut *s;
            match s.peers.get_mut(&id) {
                // Connected: only remember the address. Dialing would replace a working connection (the duplicate
                // rule), cutting off what's on it.
                Some(peer) => (
                    remember(&mut peer.addrs, addr),
                    s.conns.get(&id).is_none_or(|c| c.conn.close_reason().is_some()),
                ),
                None => {
                    remember(s.found.entry(id.clone()).or_default(), addr);
                    (false, false)
                }
            }
        };
        if new {
            self.save_peers();
        }
        if dial {
            self.connect(&id);
        }
    }

    pub async fn copy(self: &Arc<Self>, formats: Vec<Format>, mut files: Vec<File>) -> Result<(), Error> {
        let mut h = blake3::Hasher::new();
        for f in &formats {
            hash_part(&mut h, &f.mime, f.data.len() as u64);
            h.update(&f.data);
        }
        for f in &mut files {
            f.size = hash_file(&mut h, f).await?;
        }
        let id = h.finalize().to_hex().to_string();
        {
            let mut s = self.lock();
            if s.paused || s.delivered.contains(&id) {
                return Ok(());
            }
        }
        let text = formats
            .iter()
            .find(|f| f.mime.starts_with("text/plain"))
            .map(|f| store::preview(&f.data[..f.data.len().min(PREVIEW_BYTES)]));
        store::save_blob(&self.dir, &id, &formats).await?;
        let meta = Meta {
            id,
            from: self.id.clone(),
            time_ms: now_ms(),
            formats: formats
                .into_iter()
                .map(|f| FormatPart {
                    mime: f.mime,
                    size: f.data.len() as u64,
                })
                .collect(),
            files,
            text,
        };
        let (targets, forgotten) = {
            let mut s = self.lock();
            let forgotten = s.add_history(meta.clone());
            // Devices whose hello hasn't arrived yet get the item as their catch-up instead.
            let targets: Vec<(Connection, bool)> = s
                .conns
                .values()
                .filter_map(|c| Some((c.conn.clone(), c.metered?)))
                .collect();
            (targets, forgotten)
        };
        self.save_history(forgotten);
        // Each send streams the data from disk, so sending to many devices doesn't copy it in memory.
        let meta = Arc::new(meta);
        for (conn, metered) in targets {
            let (dir, meta) = (self.dir.clone(), meta.clone());
            self.spawn(async move {
                let _ = send_item(&conn, &dir, &meta, metered, false).await;
            });
        }
        Ok(())
    }

    pub async fn fetch(self: &Arc<Self>, id: &str) -> Result<Clip, Error> {
        if self.lock().history.iter().any(|h| h.id == id) {
            return self.item(id).await;
        }
        let (rx, conn, size) = {
            let mut s = self.lock();
            if s.paused {
                return Err(net("sharing is paused"));
            }
            let Some((_, from, size)) = s.offers.iter().find(|o| o.0 == id).cloned() else {
                return Err(Error::NotFound);
            };
            let Some(conn) = s.conns.get(&from).map(|c| c.conn.clone()) else {
                return Err(net("the device that has it is offline"));
            };
            let (tx, rx) = oneshot::channel();
            s.fetches.entry(id.to_string()).or_default().push(tx);
            (rx, conn, size)
        };
        // ponytail: one timeout for the whole transfer (30 s plus 1 s per MB); per-chunk progress would be tighter.
        let limit = Duration::from_secs(30 + size / 1_000_000);
        let res = async {
            send_msg(&conn, &Msg::Request { id: id.to_string() }).await?;
            // The answer comes on this connection; if it closes, it won't come.
            tokio::select! {
                got = tokio::time::timeout(limit, rx) => match got {
                    Err(_) => Err(net("timed out")),
                    Ok(Err(_)) => Err(net("the transfer failed")),
                    Ok(Ok(res)) => res,
                },
                e = conn.closed() => Err(net(e)),
            }
        };
        let res = res.await;
        if res.is_err() {
            // Only this call gave up; others waiting for the same item keep waiting.
            let mut s = self.lock();
            if let Some(waiters) = s.fetches.get_mut(id) {
                waiters.retain(|w| !w.is_closed());
                if waiters.is_empty() {
                    s.fetches.remove(id);
                }
            }
        }
        res?;
        self.item(id).await
    }

    pub async fn item(&self, id: &str) -> Result<Clip, Error> {
        let meta = self.lock().history.iter().find(|h| h.id == id).cloned();
        store::load_item(&self.dir, &meta.ok_or(Error::NotFound)?).await
    }

    pub async fn unpair(self: &Arc<Self>, id: &str) -> Result<(), Error> {
        let id = id.parse::<PublicKey>().map_err(|_| Error::UnknownDevice)?.to_string();
        let (conn, others) = {
            let mut s = self.lock();
            if s.peers.remove(&id).is_none() {
                return Err(Error::UnknownDevice);
            }
            s.removed.insert(id.clone());
            let conn = s.conns.remove(&id).map(|c| c.conn);
            (conn, s.conns.values().map(|c| c.conn.clone()).collect::<Vec<_>>())
        };
        self.save_peers();
        self.emit_devices();
        for other in others {
            let msg = Msg::Unpair { id: id.clone() };
            self.spawn(async move {
                let _ = send_msg(&other, &msg).await;
            });
        }
        if let Some(conn) = conn {
            // Tell it we forgot it, and close once it has the message.
            let told = async {
                let mut send = conn.open_uni().await.map_err(net)?;
                write_frame(&mut send, &Msg::Unpair { id: self.id.clone() }).await?;
                send.finish().map_err(net)?;
                send.stopped().await.map_err(net)
            };
            let _ = tokio::time::timeout(Duration::from_secs(3), told).await;
            conn.close(VarInt::from_u32(CLOSE_UNPAIRED), b"unpaired");
        }
        Ok(())
    }

    pub fn set_metered(self: &Arc<Self>, metered: bool) {
        let conns: Vec<Connection> = {
            let mut s = self.lock();
            if s.metered == metered {
                return;
            }
            s.metered = metered;
            s.conns.values().map(|c| c.conn.clone()).collect()
        };
        for conn in conns {
            let hello = self.hello();
            self.spawn(async move {
                let _ = send_msg(&conn, &hello).await;
            });
        }
    }

    pub async fn stop(&self) {
        let conns: Vec<Conn> = {
            let mut s = self.lock();
            s.stopped = true;
            s.conns.drain().map(|(_, c)| c).collect()
        };
        for c in conns {
            c.conn.close(VarInt::from_u32(CLOSE_STOP), b"stop");
        }
        self.ep.close().await;
    }

    /// Saves the history list, and deletes the data of items that left it. Call it without the state lock.
    fn save_history(self: &Arc<Self>, forgotten: Vec<String>) {
        {
            let _saving = self.saving.lock().unwrap_or_else(|e| e.into_inner());
            let history = self.lock().history.clone();
            store::save_history(&self.dir, &history);
        }
        if forgotten.is_empty() {
            return;
        }
        // Off the async threads: received files can be big.
        let this = self.clone();
        self.rt.spawn_blocking(move || {
            for id in forgotten {
                // It may have come back meanwhile.
                if !this.lock().history.iter().any(|h| h.id == id) {
                    store::forget(&this.dir, &id);
                }
            }
        });
    }
}

/// Binds both UDP sockets (IPv4 required, IPv6 if possible) on `port`. Relays and address lookups stay off: we
/// connect only to addresses we were given.
async fn bind(key: SecretKey, port: u16) -> Result<Endpoint, Error> {
    let v6 = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);
    // At most 4 messages at once from each device: a device that reads slowly makes the sender wait (QUIC flow
    // control) instead of piling up streams. The cost: a 5th message waits until one of the 4 is done, so small
    // messages can queue behind big items. Keep-alive every 25 s; a device silent for 60 s is gone. iroh keeps its own
    // per-path settings (keep-alive 5 s, idle 15 s at most), which it doesn't let us raise.
    let transport = QuicTransportConfig::builder()
        .max_concurrent_uni_streams(VarInt::from_u32(4))
        .keep_alive_interval(Duration::from_secs(25))
        .max_idle_timeout(IdleTimeout::try_from(Duration::from_secs(60)).ok())
        .build();
    Endpoint::builder(presets::Minimal)
        .secret_key(key)
        .transport_config(transport)
        .alpns(vec![ALPN.to_vec(), PAIR_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
        .map_err(net)?
        .bind_addr_with_opts(v6, BindOpts::default().set_is_required(false))
        .map_err(net)?
        .bind()
        .await
        .map_err(net)
}

/// Sends an item whole, or as an offer if it's too big or the device is metered and it isn't text.
async fn send_item(conn: &Connection, dir: &Path, meta: &Meta, metered: bool, latest: bool) -> Result<(), Error> {
    let size = meta.size();
    if size <= SIZE_CAP && (!metered || meta.is_text()) {
        return send_clip(conn, dir, meta, latest).await;
    }
    let offer = OfferMsg {
        id: meta.id.clone(),
        time_ms: meta.time_ms,
        size,
        mimes: meta.formats.iter().map(|f| f.mime.clone()).collect(),
        file_names: meta.files.iter().map(|f| f.name.clone()).collect(),
        latest,
    };
    send_msg(conn, &Msg::Offer(offer)).await
}

/// Sends an item, streaming its formats from `history/<id>` and its files from their paths.
async fn send_clip(conn: &Connection, dir: &Path, meta: &Meta, latest: bool) -> Result<(), Error> {
    let header = ClipMsg {
        id: meta.id.clone(),
        time_ms: meta.time_ms,
        formats: meta.formats.clone(),
        files: meta
            .files
            .iter()
            .map(|f| FilePart {
                name: f.name.clone(),
                size: f.size,
            })
            .collect(),
        latest,
    };
    let mut send = conn.open_uni().await.map_err(net)?;
    write_frame(&mut send, &Msg::Clip(header)).await?;
    let blob = (store::blob_path(dir, &meta.id), meta.formats_size());
    let files = meta.files.iter().map(|f| (PathBuf::from(&f.path), f.size));
    for (path, size) in std::iter::once(blob).chain(files) {
        let file = tokio::fs::File::open(&path).await.map_err(io)?;
        if tokio::io::copy(&mut file.take(size), &mut send).await.map_err(io)? != size {
            return Err(io("a file changed while sending"));
        }
    }
    send.finish().map_err(net)
}

/// Saves an item's formats to `path` as they arrive, adding them to the item id. Returns the start of its plain text.
async fn receive_formats(
    recv: &mut RecvStream,
    h: &mut blake3::Hasher,
    path: &Path,
    parts: &[FormatPart],
) -> Result<Option<String>, Error> {
    let mut out = tokio::fs::File::create(path).await.map_err(io)?;
    let mut text = None;
    for part in parts {
        let mut head = Vec::new();
        let want = text.is_none() && part.mime.starts_with("text/plain");
        receive_part(recv, h, &part.mime, part.size, &mut out, want.then_some(&mut head)).await?;
        if want {
            text = Some(store::preview(&head));
        }
    }
    out.flush().await.map_err(io)?;
    Ok(text)
}

/// Copies one format or file of `len` bytes from the stream to `out`, adding it to the item id. Its first
/// `PREVIEW_BYTES` also go to `head`.
async fn receive_part(
    recv: &mut RecvStream,
    h: &mut blake3::Hasher,
    name: &str,
    len: u64,
    out: &mut tokio::fs::File,
    mut head: Option<&mut Vec<u8>>,
) -> Result<(), Error> {
    hash_part(h, name, len);
    let mut buf = vec![0; 64 * 1024];
    let mut left = len;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        recv.read_exact(&mut buf[..n]).await.map_err(net)?;
        h.update(&buf[..n]);
        out.write_all(&buf[..n]).await.map_err(io)?;
        if let Some(head) = head.as_deref_mut() {
            let room = PREVIEW_BYTES.saturating_sub(head.len());
            head.extend_from_slice(&buf[..n.min(room)]);
        }
        left -= n as u64;
    }
    Ok(())
}

/// Saves an item's files to `tmp` as they arrive, adding them to the item id. Their paths are in `dir`, where the
/// caller moves them.
async fn receive_files(
    recv: &mut RecvStream,
    h: &mut blake3::Hasher,
    tmp: &Path,
    dir: &Path,
    parts: Vec<FilePart>,
) -> Result<Vec<File>, Error> {
    let mut files = Vec::new();
    let mut names = HashSet::new();
    for (i, part) in parts.into_iter().enumerate() {
        tokio::fs::create_dir_all(tmp).await.map_err(io)?;
        // Ignoring case, as on macOS, where "A" and "a" are the same file.
        let mut name = store::safe_name(&part.name);
        while !names.insert(name.to_lowercase()) {
            name = format!("{i}-{name}");
        }
        let path = dir.join(&name);
        let mut out = tokio::fs::File::create(tmp.join(&name)).await.map_err(io)?;
        receive_part(recv, h, &part.name, part.size, &mut out, None).await?;
        out.flush().await.map_err(io)?;
        files.push(File {
            name: part.name,
            path: path.to_string_lossy().into_owned(),
            size: part.size,
        });
    }
    Ok(files)
}

/// Adds a file to the item id, reading it from disk. Returns its size.
async fn hash_file(h: &mut blake3::Hasher, file: &File) -> Result<u64, Error> {
    let mut f = tokio::fs::File::open(&file.path).await.map_err(io)?;
    let size = f.metadata().await.map_err(io)?.len();
    hash_part(h, &file.name, size);
    let mut buf = vec![0; 64 * 1024];
    let mut left = size;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want]).await.map_err(io)?;
        if n == 0 {
            return Err(io("a file changed while reading it"));
        }
        h.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(size)
}

pub fn parse_addrs(addrs: &[String]) -> Vec<SocketAddr> {
    addrs.iter().filter_map(|a| a.parse().ok()).collect()
}

/// Puts `addr` first in the list. Returns whether it's new.
fn remember(addrs: &mut Vec<String>, addr: SocketAddr) -> bool {
    let addr = addr.to_string();
    let new = !addrs.contains(&addr);
    addrs.retain(|a| *a != addr);
    addrs.insert(0, addr);
    addrs.truncate(MAX_ADDRS);
    new
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
