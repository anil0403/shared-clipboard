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
    endpoint::{BindOpts, Connection, IdleTimeout, IncomingAddr, QuicTransportConfig, Side, VarInt, presets},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::oneshot,
    time::Instant,
};

use crate::{
    Clip, Config, DEFAULT_PORT, Device, Error, Events, File, Format, Offer, PROTOCOL_VERSION, Platform, SIZE_CAP,
    bluetooth::{self, BLE_CAP, Ble, BleConn, ble_limit},
    pairing::{self, Link},
    protocol::{
        ALPN, BODY_SLACK, ClipMsg, FilePart, FormatPart, MAX_FILE_NAME, MAX_HEADER, MAX_NAME, Msg, OfferMsg, PAIR_ALPN,
        PeerInfo, clean, hash_part, net, read_frame, send_msg, valid_id, write_frame,
    },
    store::{self, HISTORY_LEN, Meta, PREVIEW_BYTES, io},
};

const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Waits between dials to an offline device: the first, doubling up to the last.
const REDIAL_FIRST: Duration = Duration::from_secs(5);
const REDIAL_MAX: Duration = Duration::from_secs(5 * 60);
/// How many addresses we keep for each device, newest first.
const MAX_ADDRS: usize = 8;
/// How many offers we remember for `fetch`.
const MAX_OFFERS: usize = 50;
/// How many items given to the app we remember, so the app reporting them back doesn't send them again.
const MAX_DELIVERED: usize = 8;
/// Two connections made within this time of each other are a simultaneous dial; a later one is a reconnect.
const SIMULTANEOUS: Duration = Duration::from_secs(3);
/// How long a device has to answer when `network_change` checks its connection.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

// Why we closed a connection.
const CLOSE_STOP: u32 = 0;
const CLOSE_DUPLICATE: u32 = 1;
const CLOSE_NOT_PAIRED: u32 = 2;
const CLOSE_UNPAIRED: u32 = 3;
const CLOSE_SILENT: u32 = 4;

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
    /// Wakes `redial_loop`: a device went offline, the network changed, or the node stopped.
    wake: tokio::sync::Notify,
    /// Held while `copy` adds an item to the history, so items go in the order the calls start.
    copying: tokio::sync::Mutex<()>,
    /// Held while writing the state's files, so the last write has the newest state.
    saving: Mutex<()>,
}

#[derive(Default)]
pub struct State {
    /// Paired devices, by id.
    pub peers: BTreeMap<String, PeerInfo>,
    /// The connection to each paired device that's online.
    conns: HashMap<String, Conn>,
    /// The Bluetooth link to each paired device that has one. Used only while there's no connection in `conns`.
    pub(crate) ble: HashMap<String, BleConn>,
    /// How long a Bluetooth link may receive nothing before it's closed.
    pub(crate) ble_idle: Duration,
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
    pub(crate) stopped: bool,
    /// When to dial each offline device next, and the wait after that.
    redial: HashMap<String, (Instant, Duration)>,
    /// Devices the user unpaired: another device listing them (`peers`, `paired`) doesn't bring them back.
    removed: HashSet<String>,
}

struct Conn {
    conn: Connection,
    /// Whether the device is on a metered network; `None` until its `hello` arrives.
    metered: Option<bool>,
    /// When it was added.
    since: Instant,
}

impl State {
    pub fn devices(&self) -> Vec<Device> {
        let device = |p: &PeerInfo| {
            let (net, ble) = (self.conns.contains_key(&p.id), self.ble.contains_key(&p.id));
            Device {
                id: p.id.clone(),
                name: p.name.clone(),
                platform: p.platform,
                online: net || ble,
                bluetooth: ble && !net,
            }
        };
        self.peers.values().map(device).collect()
    }

    /// Some paired device isn't connected over the network.
    pub fn bluetooth_wanted(&self) -> bool {
        self.peers.keys().any(|id| !self.conns.contains_key(id))
    }

    /// The link to each device that's online: the network if it's up, else Bluetooth. With what its `hello` said
    /// about a metered network, `None` until it arrives.
    fn links(&self) -> impl Iterator<Item = (&String, Via, Option<bool>)> {
        let net = self
            .conns
            .iter()
            .map(|(id, c)| (id, Via::Net(c.conn.clone()), c.metered));
        let ble = self.ble.iter().filter(|(id, _)| !self.conns.contains_key(*id));
        net.chain(ble.map(|(id, b)| (id, Via::Ble(b.ble.clone()), b.metered)))
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
            ble_idle: bluetooth::IDLE,
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
            wake: tokio::sync::Notify::new(),
            copying: tokio::sync::Mutex::new(()),
            saving: Mutex::new(()),
        });
        inner.spawn(inner.clone().accept_loop());
        inner.spawn(inner.clone().redial_loop());
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
            // Two connections: if both devices dialed at once, keep the one opened by the lower id. A later one is a
            // reconnect, and the old one may be dead without knowing it yet (iOS takes a suspended app's sockets
            // away): keep the newer.
            let old = s.conns.get(&id).filter(|c| c.conn.close_reason().is_none());
            let simultaneous = old.is_some_and(|c| c.since.elapsed() < SIMULTANEOUS);
            let old = old.map(|c| c.conn.clone());
            match &old {
                Some(old) if simultaneous && opener(old) < opener(&conn) => {
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
                        since: Instant::now(),
                    };
                    s.conns.insert(id.clone(), c);
                    s.redial.remove(&id);
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

    pub fn hello(&self) -> Msg {
        let metered = self.lock().metered;
        Msg::Hello {
            name: self.name.clone(),
            platform: self.platform,
            protocol: PROTOCOL_VERSION,
            features: if metered { vec!["metered".into()] } else { vec![] },
        }
    }

    async fn read_loop(self: Arc<Self>, id: String, conn: Connection) {
        while let Ok(mut recv) = conn.accept_uni().await {
            let (this, id, via) = (self.clone(), id.clone(), Via::Net(conn.clone()));
            self.spawn(async move {
                if let Ok(msg) = read_frame(&mut recv, MAX_HEADER).await {
                    let _ = this.handle(&id, &via, msg, recv).await;
                }
            });
        }
        {
            let mut s = self.lock();
            if s.conns.get(&id).is_none_or(|c| c.conn.stable_id() != conn.stable_id()) {
                return;
            }
            s.conns.remove(&id);
        }
        self.emit_devices();
        self.wake.notify_one();
    }

    /// Dials paired devices that are offline: at once, then after 5 s, 10 s, 20 s... up to 5 minutes apart. A
    /// connection starts the waits over, and `network_change` dials them all now.
    async fn redial_loop(self: Arc<Self>) {
        loop {
            let (due, next) = {
                let mut s = self.lock();
                if s.stopped {
                    return;
                }
                let s = &mut *s;
                let now = Instant::now();
                s.redial
                    .retain(|id, _| s.peers.contains_key(id) && !s.conns.contains_key(id));
                let mut due = Vec::new();
                for id in s.peers.keys().filter(|id| !s.conns.contains_key(*id)) {
                    let (at, wait) = s.redial.entry(id.clone()).or_insert((now, REDIAL_FIRST));
                    if *at <= now {
                        *at = now + *wait;
                        *wait = (*wait * 2).min(REDIAL_MAX);
                        due.push(id.clone());
                    }
                }
                (due, s.redial.values().map(|r| r.0).min())
            };
            for id in due {
                self.connect(&id);
            }
            match next {
                Some(at) => _ = tokio::time::timeout_at(at, self.wake.notified()).await,
                None => self.wake.notified().await,
            }
        }
    }

    /// The network changed, or the phone woke: let iroh check its paths, drop the connections that don't answer, and
    /// dial every offline device now. QUIC's idle timer runs on a clock that stops while a phone sleeps, so without the
    /// check a phone that slept away from home shows its devices online for up to another minute awake.
    pub async fn network_change(&self) {
        self.ep.network_change().await;
        let conns: Vec<Connection> = self.lock().conns.values().map(|c| c.conn.clone()).collect();
        let mut checks = tokio::task::JoinSet::new();
        for conn in conns {
            let hello = self.hello();
            checks.spawn(async move {
                // Any packet back (the ack for our hello is enough) means it's there.
                let heard = conn.stats().udp_rx.datagrams;
                let answered = tokio::time::timeout(CHECK_TIMEOUT, async {
                    let _ = send_msg(&conn, &hello).await;
                    while conn.stats().udp_rx.datagrams == heard {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await;
                if answered.is_err() {
                    conn.close(VarInt::from_u32(CLOSE_SILENT), b"silent");
                }
            });
        }
        checks.join_all().await;
        self.lock().redial.clear();
        self.wake.notify_one();
    }

    /// One incoming message, on link `via`. A `clip`'s body comes from `body`.
    pub async fn handle(
        self: &Arc<Self>,
        from: &str,
        via: &Via,
        msg: Msg,
        body: impl AsyncRead + Unpin,
    ) -> Result<(), Error> {
        match msg {
            Msg::Hello {
                name,
                platform,
                features,
                ..
            } => self.on_hello(
                from,
                via,
                clean(&name, MAX_NAME),
                platform,
                features.iter().any(|f| f == "metered"),
            ),
            Msg::Clip(m) => self.on_clip(from, body, m).await?,
            Msg::Offer(m) => self.on_offer(from, m),
            Msg::Request { id } => self.on_request(from, via, &id),
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

    fn on_hello(self: &Arc<Self>, from: &str, via: &Via, name: String, platform: Platform, metered: bool) {
        let (renamed, first) = {
            let mut s = self.lock();
            let s = &mut *s;
            let mut renamed = false;
            if let Some(p) = s.peers.get_mut(from)
                && (p.name != name || p.platform != platform)
            {
                (p.name, p.platform, renamed) = (name, platform, true);
            }
            let on_net = s.conns.contains_key(from);
            let first = match (via, s.conns.get_mut(from), s.ble.get_mut(from)) {
                (Via::Net(conn), Some(c), _) if c.conn.stable_id() == conn.stable_id() => {
                    c.metered.replace(metered).is_none()
                }
                // Over Bluetooth only while there's no network connection: that one did the catch-up.
                (Via::Ble(ble), _, Some(b)) if b.ble.link == ble.link => {
                    b.metered.replace(metered).is_none() && !on_net
                }
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
                s.history.first().filter(|_| !s.paused).cloned()
            };
            if let Some(meta) = latest {
                let (this, via) = (self.clone(), via.clone());
                self.spawn(async move {
                    let _ = via.send_item(&this.dir, &meta, metered, true).await;
                });
            }
        }
    }

    async fn on_clip(self: &Arc<Self>, from: &str, recv: impl AsyncRead + Unpin, m: ClipMsg) -> Result<(), Error> {
        let id = m.id.clone();
        let res = self.receive_clip(from, recv, m).await;
        if res.is_err() {
            // A `fetch` waiting for it fails now, not at its timeout.
            self.lock().fetches.remove(&id);
        }
        res
    }

    async fn receive_clip(
        self: &Arc<Self>,
        from: &str,
        mut recv: impl AsyncRead + Unpin,
        m: ClipMsg,
    ) -> Result<(), Error> {
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

    /// Sends the item asked for on the link the request came on, or `missing` if we don't have it, are paused, or it
    /// can't go over that link (Bluetooth: over 1 MiB or with files).
    fn on_request(&self, from: &str, via: &Via, id: &str) {
        let meta = {
            let s = self.lock();
            if !s.peers.contains_key(from) {
                return;
            }
            let fits = |m: &&Meta| matches!(via, Via::Net(_)) || (m.files.is_empty() && m.size() <= BLE_CAP);
            s.history
                .iter()
                .find(|h| h.id == id)
                .filter(fits)
                .filter(|_| !s.paused)
                .cloned()
        };
        let (dir, id, via) = (self.dir.clone(), id.to_string(), via.clone());
        self.spawn(async move {
            let _ = match meta {
                Some(meta) => via.send_clip(&dir, &meta, false).await,
                None => via.send_msg(&Msg::Missing { id }).await,
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
        let links: Vec<Via> = {
            let s = self.lock();
            s.links()
                .filter(|(id, ..)| **id != peer.id)
                .map(|(_, via, _)| via)
                .collect()
        };
        for via in links {
            let msg = Msg::Peers {
                devices: vec![peer.clone()],
            };
            self.spawn(async move {
                let _ = via.send_msg(&msg).await;
            });
        }
    }

    /// Forgets a device and closes its links.
    fn forget(&self, id: &str) {
        let (conn, ble) = {
            let mut s = self.lock();
            if s.peers.remove(id).is_none() {
                return;
            }
            s.removed.insert(id.to_string());
            (s.conns.remove(id), s.ble.remove(id))
        };
        self.save_peers();
        if let Some(c) = conn {
            c.conn.close(VarInt::from_u32(CLOSE_UNPAIRED), b"unpaired");
        }
        if let Some(b) = ble {
            b.ble.close();
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
        // Copies go into the history in the order the calls start (the lock is fair), even if one takes longer to
        // read its files.
        let in_order = self.copying.lock().await;
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
            let s = self.lock();
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
            let targets: Vec<(Via, bool)> = s
                .links()
                .filter_map(|(_, via, metered)| Some((via, metered?)))
                .collect();
            (targets, forgotten)
        };
        drop(in_order);
        self.save_history(forgotten);
        // Each network send streams the data from disk, so sending to many devices doesn't copy it in memory.
        let mut limit = transfer_limit(meta.size());
        if targets.iter().any(|(via, _)| matches!(via, Via::Ble(_))) {
            limit = limit.max(ble_limit(meta.size().min(BLE_CAP)));
        }
        let meta = Arc::new(meta);
        let sends: Vec<_> = targets
            .into_iter()
            .map(|(via, metered)| {
                let (dir, meta) = (self.dir.clone(), meta.clone());
                self.rt
                    .spawn(async move { via.send_item(&dir, &meta, metered, false).await })
            })
            .collect();
        // Wait until each device has it, turned it down or went away. Sends still running after the limit go on.
        let all = async {
            for send in sends {
                let _ = send.await;
            }
        };
        tokio::time::timeout(limit, all)
            .await
            .map_err(|_| net("not every device has it yet"))
    }

    pub async fn fetch(self: &Arc<Self>, id: &str) -> Result<Clip, Error> {
        if self.lock().history.iter().any(|h| h.id == id) {
            return self.item(id).await;
        }
        let (rx, via, size) = {
            let mut s = self.lock();
            if s.paused {
                return Err(net("sharing is paused"));
            }
            let Some((_, from, size)) = s.offers.iter().find(|o| o.0 == id).cloned() else {
                return Err(Error::NotFound);
            };
            let Some((_, via, _)) = s.links().find(|(id, ..)| **id == from) else {
                return Err(net("the device that has it is offline"));
            };
            let (tx, rx) = oneshot::channel();
            s.fetches.entry(id.to_string()).or_default().push(tx);
            (rx, via, size)
        };
        let limit = match via {
            Via::Net(_) => transfer_limit(size),
            Via::Ble(_) => ble_limit(size.min(BLE_CAP)),
        };
        let res = async {
            via.send_msg(&Msg::Request { id: id.to_string() }).await?;
            // The answer comes on this link; if it closes, it won't come.
            tokio::select! {
                got = tokio::time::timeout(limit, rx) => match got {
                    Err(_) => Err(net("timed out")),
                    Ok(Err(_)) => Err(net("the transfer failed")),
                    Ok(Ok(res)) => res,
                },
                e = via.closed() => Err(e),
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
        let (conn, ble, others) = {
            let mut s = self.lock();
            if s.peers.remove(&id).is_none() {
                return Err(Error::UnknownDevice);
            }
            s.removed.insert(id.clone());
            let conn = s.conns.remove(&id).map(|c| c.conn);
            let ble = s.ble.remove(&id).map(|b| b.ble);
            (conn, ble, s.links().map(|(_, via, _)| via).collect::<Vec<_>>())
        };
        self.save_peers();
        self.emit_devices();
        for other in others {
            let msg = Msg::Unpair { id: id.clone() };
            self.spawn(async move {
                let _ = other.send_msg(&msg).await;
            });
        }
        if let Some(ble) = ble {
            if conn.is_none() {
                let told = Msg::Unpair { id: self.id.clone() };
                let _ = tokio::time::timeout(Duration::from_secs(3), ble.send_msg(&told)).await;
            }
            ble.close();
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
            s.ble.drain().for_each(|(_, b)| b.ble.close());
            s.conns.drain().map(|(_, c)| c).collect()
        };
        self.wake.notify_one();
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

/// The link to a device: a message came on it, or goes out on it.
#[derive(Clone)]
pub enum Via {
    Net(Connection),
    Ble(Ble),
}

impl Via {
    async fn send_msg(&self, msg: &Msg) -> Result<(), Error> {
        match self {
            Via::Net(conn) => send_msg(conn, msg).await,
            Via::Ble(ble) => ble.send_msg(msg).await,
        }
    }

    /// Sends an item whole, or as an offer: on the network if it's too big, or the device is metered and it isn't
    /// text; over Bluetooth if it's over 1 MiB or has files.
    async fn send_item(&self, dir: &Path, meta: &Meta, metered: bool, latest: bool) -> Result<(), Error> {
        let size = meta.size();
        let whole = match self {
            Via::Net(_) => size <= SIZE_CAP && (!metered || meta.is_text()),
            Via::Ble(_) => size <= BLE_CAP && meta.files.is_empty(),
        };
        if whole {
            return self.send_clip(dir, meta, latest).await;
        }
        let offer = OfferMsg {
            id: meta.id.clone(),
            time_ms: meta.time_ms,
            size,
            mimes: meta.formats.iter().map(|f| f.mime.clone()).collect(),
            file_names: meta.files.iter().map(|f| f.name.clone()).collect(),
            latest,
        };
        self.send_msg(&Msg::Offer(offer)).await
    }

    async fn send_clip(&self, dir: &Path, meta: &Meta, latest: bool) -> Result<(), Error> {
        match self {
            Via::Net(conn) => send_clip(conn, dir, meta, latest).await,
            // At most 1 MiB, so it's read into memory.
            Via::Ble(ble) => {
                let mut frame = Vec::new();
                write_clip(&mut frame, dir, meta, latest).await?;
                ble.send(frame).await
            }
        }
    }

    /// Waits until the link closes.
    async fn closed(&self) -> Error {
        match self {
            Via::Net(conn) => net(conn.closed().await),
            Via::Ble(ble) => {
                ble.closed().await;
                net("the Bluetooth link closed")
            }
        }
    }
}

/// Sends an item on its own stream.
async fn send_clip(conn: &Connection, dir: &Path, meta: &Meta, latest: bool) -> Result<(), Error> {
    let mut send = conn.open_uni().await.map_err(net)?;
    write_clip(&mut send, dir, meta, latest).await?;
    send.finish().map_err(net)?;
    // Done when the device has it all (or turned it down).
    send.stopped().await.map_err(net)?;
    Ok(())
}

/// Writes an item's header and body, streaming its formats from `history/<id>` and its files from their paths.
async fn write_clip(send: &mut (impl AsyncWrite + Unpin), dir: &Path, meta: &Meta, latest: bool) -> Result<(), Error> {
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
    write_frame(send, &Msg::Clip(header)).await?;
    let blob = (store::blob_path(dir, &meta.id), meta.formats_size());
    let files = meta.files.iter().map(|f| (PathBuf::from(&f.path), f.size));
    for (path, size) in std::iter::once(blob).chain(files) {
        let file = tokio::fs::File::open(&path).await.map_err(io)?;
        if tokio::io::copy(&mut file.take(size), send).await.map_err(io)? != size {
            return Err(io("a file changed while sending"));
        }
    }
    Ok(())
}

/// How long a whole transfer of `size` bytes may take: 30 s plus 1 s per MB.
// ponytail: one timeout for the whole transfer; per-chunk progress would be tighter.
fn transfer_limit(size: u64) -> Duration {
    Duration::from_secs(30 + size / 1_000_000)
}

/// Saves an item's formats to `path` as they arrive, adding them to the item id. Returns the start of its plain text.
async fn receive_formats(
    recv: &mut (impl AsyncRead + Unpin),
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
    recv: &mut (impl AsyncRead + Unpin),
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
    recv: &mut (impl AsyncRead + Unpin),
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
