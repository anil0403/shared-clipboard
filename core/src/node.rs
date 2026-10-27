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
    endpoint::{BindOpts, Connection, ConnectionError, IncomingAddr, RecvStream, Side, VarInt, presets},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
};

use crate::{
    Clip, Config, DEFAULT_PORT, Device, Error, Events, File, Format, Offer, PROTOCOL_VERSION, Platform, SIZE_CAP,
    pairing::{self, Link},
    protocol::{
        ALPN, BODY_SLACK, ClipMsg, FilePart, FormatPart, MAX_HEADER, Msg, OfferMsg, PAIR_ALPN, PeerInfo, hash_part,
        is_text, net, read_frame, send_msg, size, valid_id, write_frame,
    },
    store::{self, HISTORY_LEN, io},
};

const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// How many addresses we keep for each device, newest first.
const MAX_ADDRS: usize = 8;
/// How many offers we remember for `fetch`.
const MAX_OFFERS: usize = 50;

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
    /// Newest first.
    pub history: Vec<Clip>,
    /// The last item given to the app; when the app reports it back, it isn't sent again.
    last_delivered: Option<String>,
    /// Offers we got, newest first: (item id, device that has it, size).
    offers: VecDeque<(String, String, u64)>,
    /// `fetch` calls waiting for an item.
    fetches: HashMap<String, Vec<oneshot::Sender<Clip>>>,
    metered: bool,
    pub paused: bool,
    pub link: Option<Link>,
    /// A pairing is running.
    pub pairing: bool,
    /// The user's answer to a pairing code, by device id.
    pub confirms: HashMap<String, oneshot::Sender<bool>>,
    stopped: bool,
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
        let key = store::load_key(&dir)?;
        let ep = match bind(key.clone(), if config.port == 0 { DEFAULT_PORT } else { config.port }).await {
            Err(_) if config.port == 0 => bind(key, 0).await?,
            ep => ep?,
        };
        let port = ep.bound_sockets().iter().find(|a| a.is_ipv4()).map_or(0, |a| a.port());
        let state = State {
            peers: store::load_peers(&dir).into_iter().map(|p| (p.id.clone(), p)).collect(),
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

    /// Dials paired device `id` at the addresses we know, unless a dial is already running.
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
            let addr = EndpointAddr::from_parts(key, addrs.into_iter().map(TransportAddr::Ip));
            let conn = tokio::time::timeout(DIAL_TIMEOUT, this.ep.connect(addr, ALPN)).await;
            this.lock().dialing.remove(&id);
            if let Ok(Ok(conn)) = conn {
                this.add_conn(conn, None);
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
        let came_online = {
            let mut s = self.lock();
            let s = &mut *s;
            let Some(peer) = s.peers.get_mut(&id).filter(|_| !s.stopped) else {
                conn.close(VarInt::from_u32(CLOSE_NOT_PAIRED), b"not paired");
                return;
            };
            if let Some(addr) = addr
                && remember(&mut peer.addrs, addr)
            {
                store::save_peers(&self.dir, s.peers.values());
            }
            // Two connections: keep the one opened by the lower id (or the newer, if one device opened both).
            let old = s
                .conns
                .get(&id)
                .map(|c| c.conn.clone())
                .filter(|c| c.close_reason().is_none());
            if let Some(old) = &old {
                if opener(old) < opener(&conn) {
                    conn.close(VarInt::from_u32(CLOSE_DUPLICATE), b"duplicate");
                    return;
                }
                old.close(VarInt::from_u32(CLOSE_DUPLICATE), b"duplicate");
            }
            s.conns.insert(
                id.clone(),
                Conn {
                    conn: conn.clone(),
                    metered: None,
                },
            );
            old.is_none()
        };
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
            } => self.on_hello(from, cid, name, platform, features.iter().any(|f| f == "metered")),
            Msg::Clip(m) => self.on_clip(from, recv, m).await?,
            Msg::Offer(m) => self.on_offer(from, m),
            Msg::Request { id } => self.on_request(from, &id),
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
                store::save_peers(&self.dir, s.peers.values());
            }
            let first = match s.conns.get_mut(from) {
                Some(c) if c.conn.stable_id() == cid => c.metered.replace(metered).is_none(),
                _ => false,
            };
            (renamed, first)
        };
        if renamed {
            self.emit_devices();
        }
        // Catch-up: once we know what the device accepts, send it our newest item.
        if first {
            let latest = {
                let s = self.lock();
                let latest = s.history.first().zip(s.conns.get(from)).filter(|_| !s.paused);
                latest.map(|(clip, c)| (clip.clone(), c.conn.clone()))
            };
            if let Some((clip, conn)) = latest {
                self.spawn(async move {
                    let _ = send_item(&conn, &clip, metered, true).await;
                });
            }
        }
    }

    async fn on_clip(self: &Arc<Self>, from: &str, mut recv: RecvStream, m: ClipMsg) -> Result<(), Error> {
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
            if total > cap + BODY_SLACK {
                return Err(net("item too big"));
            }
            if !fetching && (s.paused || (m.latest && !s.is_newer(m.time_ms))) {
                return Ok(());
            }
        }

        let mut h = blake3::Hasher::new();
        let mut formats = Vec::new();
        for f in m.formats {
            let mut data = vec![0; usize::try_from(f.size).map_err(net)?];
            recv.read_exact(&mut data).await.map_err(net)?;
            hash_part(&mut h, &f.mime, f.size);
            h.update(&data);
            formats.push(Format { mime: f.mime, data });
        }
        let dir = self.dir.join("files").join(&m.id);
        let files = match receive_files(&mut recv, &mut h, &dir, m.files).await {
            Ok(files) if h.finalize().to_hex().as_str() == m.id => files,
            _ => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(net("the item didn't match its id"));
            }
        };
        let clip = Clip {
            id: m.id,
            from: from.to_string(),
            time_ms: m.time_ms,
            formats,
            files,
        };
        store::save_blob(&self.dir, &clip).await?;

        let waiters = {
            let mut s = self.lock();
            let waiters = s.fetches.remove(&clip.id);
            if waiters.is_none() && (s.paused || (m.latest && !s.is_newer(clip.time_ms))) {
                return Ok(());
            }
            s.last_delivered = Some(clip.id.clone());
            self.add_history(&mut s, clip.clone());
            waiters
        };
        match waiters {
            Some(waiters) => waiters.into_iter().for_each(|w| _ = w.send(clip.clone())),
            None => self.events.on_clip(clip),
        }
        Ok(())
    }

    fn on_offer(&self, from: &str, m: OfferMsg) {
        {
            let mut s = self.lock();
            let have = s.history.iter().any(|c| c.id == m.id);
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
            file_names: m.file_names,
        };
        self.events.on_offer(offer);
    }

    fn on_request(&self, from: &str, id: &str) {
        let found = {
            let s = self.lock();
            let clip = s.history.iter().find(|c| c.id == id).filter(|_| !s.paused);
            clip.zip(s.conns.get(from))
                .map(|(clip, c)| (clip.clone(), c.conn.clone()))
        };
        if let Some((clip, conn)) = found {
            self.spawn(async move {
                let _ = send_clip(&conn, &clip, false).await;
            });
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

    /// Trusts these devices. Known ones only get their addresses updated. Returns the ids that are new.
    pub fn add_peers(&self, devices: Vec<PeerInfo>) -> Vec<String> {
        let mut added = Vec::new();
        let mut s = self.lock();
        for mut d in devices {
            let Ok(key) = d.id.parse::<PublicKey>() else { continue };
            d.id = key.to_string();
            if d.id == self.id {
                continue;
            }
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
        store::save_peers(&self.dir, s.peers.values());
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
            store::save_peers(&self.dir, s.peers.values());
            s.conns.remove(id)
        };
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
        let dial = {
            let mut s = self.lock();
            let s = &mut *s;
            match s.peers.get_mut(&id) {
                Some(peer) => {
                    let new = remember(&mut peer.addrs, addr);
                    if new {
                        store::save_peers(&self.dir, s.peers.values());
                    }
                    new || !s.conns.contains_key(&id)
                }
                None => {
                    remember(s.found.entry(id.clone()).or_default(), addr);
                    false
                }
            }
        };
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
            if s.paused || s.last_delivered.as_deref() == Some(id.as_str()) {
                return Ok(());
            }
            s.last_delivered = None;
        }
        let clip = Clip {
            id,
            from: self.id.clone(),
            time_ms: now_ms(),
            formats,
            files,
        };
        store::save_blob(&self.dir, &clip).await?;
        let targets: Vec<(Connection, bool)> = {
            let mut s = self.lock();
            self.add_history(&mut s, clip.clone());
            // Devices whose hello hasn't arrived yet get the item as their catch-up instead.
            s.conns
                .values()
                .filter_map(|c| Some((c.conn.clone(), c.metered?)))
                .collect()
        };
        for (conn, metered) in targets {
            let clip = clip.clone();
            self.spawn(async move {
                let _ = send_item(&conn, &clip, metered, false).await;
            });
        }
        Ok(())
    }

    pub async fn fetch(self: &Arc<Self>, id: &str) -> Result<Clip, Error> {
        let (rx, conn, size) = {
            let mut s = self.lock();
            if let Some(clip) = s.history.iter().find(|c| c.id == id) {
                return Ok(clip.clone());
            }
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
            tokio::time::timeout(limit, rx).await.map_err(net)?.map_err(net)
        };
        let res = res.await;
        if res.is_err() {
            self.lock().fetches.remove(id);
        }
        res
    }

    pub async fn unpair(self: &Arc<Self>, id: &str) -> Result<(), Error> {
        let id = id.parse::<PublicKey>().map_err(|_| Error::UnknownDevice)?.to_string();
        let (conn, others) = {
            let mut s = self.lock();
            if s.peers.remove(&id).is_none() {
                return Err(Error::UnknownDevice);
            }
            store::save_peers(&self.dir, s.peers.values());
            let conn = s.conns.remove(&id).map(|c| c.conn);
            (conn, s.conns.values().map(|c| c.conn.clone()).collect::<Vec<_>>())
        };
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

    /// Puts an item first in the history and saves the list. Its data must already be saved (`store::save_blob`).
    // ponytail: the whole history is kept in memory and written under the state lock; fine for 50 clipboard items.
    fn add_history(&self, s: &mut State, clip: Clip) {
        s.history.retain(|c| c.id != clip.id);
        s.history.insert(0, clip);
        if s.history.len() > HISTORY_LEN {
            for old in s.history.split_off(HISTORY_LEN) {
                store::forget(&self.dir, &old.id);
            }
        }
        store::save_history(&self.dir, &s.history);
    }
}

/// Binds both UDP sockets (IPv4 required, IPv6 if possible) on `port`. Relays and address lookups stay off: we
/// connect only to addresses we were given.
async fn bind(key: SecretKey, port: u16) -> Result<Endpoint, Error> {
    let v6 = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);
    Endpoint::builder(presets::Minimal)
        .secret_key(key)
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
async fn send_item(conn: &Connection, clip: &Clip, metered: bool, latest: bool) -> Result<(), Error> {
    let size = size(clip);
    if size <= SIZE_CAP && (!metered || is_text(clip)) {
        return send_clip(conn, clip, latest).await;
    }
    let offer = OfferMsg {
        id: clip.id.clone(),
        time_ms: clip.time_ms,
        size,
        mimes: clip.formats.iter().map(|f| f.mime.clone()).collect(),
        file_names: clip.files.iter().map(|f| f.name.clone()).collect(),
        latest,
    };
    send_msg(conn, &Msg::Offer(offer)).await
}

async fn send_clip(conn: &Connection, clip: &Clip, latest: bool) -> Result<(), Error> {
    let header = ClipMsg {
        id: clip.id.clone(),
        time_ms: clip.time_ms,
        formats: clip
            .formats
            .iter()
            .map(|f| FormatPart {
                mime: f.mime.clone(),
                size: f.data.len() as u64,
            })
            .collect(),
        files: clip
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
    for f in &clip.formats {
        send.write_all(&f.data).await.map_err(net)?;
    }
    for f in &clip.files {
        let file = tokio::fs::File::open(&f.path).await.map_err(io)?;
        if tokio::io::copy(&mut file.take(f.size), &mut send).await.map_err(io)? != f.size {
            return Err(io("a file changed while sending"));
        }
    }
    send.finish().map_err(net)
}

/// Saves an item's files to `dir` as they arrive, adding them to the item id.
async fn receive_files(
    recv: &mut RecvStream,
    h: &mut blake3::Hasher,
    dir: &Path,
    parts: Vec<FilePart>,
) -> Result<Vec<File>, Error> {
    let mut files = Vec::new();
    let mut names = HashSet::new();
    let mut buf = vec![0; 64 * 1024];
    for (i, part) in parts.into_iter().enumerate() {
        tokio::fs::create_dir_all(dir).await.map_err(io)?;
        let mut name = store::safe_name(&part.name);
        if !names.insert(name.clone()) {
            name = format!("{i}-{name}");
        }
        let path = dir.join(&name);
        let mut out = tokio::fs::File::create(&path).await.map_err(io)?;
        hash_part(h, &part.name, part.size);
        let mut left = part.size;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            recv.read_exact(&mut buf[..n]).await.map_err(net)?;
            h.update(&buf[..n]);
            out.write_all(&buf[..n]).await.map_err(io)?;
            left -= n as u64;
        }
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
