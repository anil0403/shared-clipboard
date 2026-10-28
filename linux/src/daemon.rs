//! `sharedclip daemon`: runs the node, syncs the clipboard, finds devices with mDNS and answers the CLI on the
//! control socket (one request per connection, plain text lines).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use sharedclip::{Clip, Config, Device, Error, Events, Node, Offer, Platform, Summary};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;

use crate::clip;

const SERVICE: &str = "_sharedclip._udp.local.";
// ponytail: offers up to this size are fetched right away, bigger ones only logged; a notification with a Download
// button comes later.
const FETCH_LIMIT: u64 = 100 * 1024 * 1024;
const CODE_TIMEOUT: Duration = Duration::from_secs(120);

/// What the `pair` sessions on the control socket hear about.
#[derive(Clone)]
enum Ev {
    Code(Device, String),
    Devices(Vec<Device>),
}

struct App {
    node: OnceLock<Arc<Node>>,
    events: broadcast::Sender<Ev>,
    /// The pairing code waiting for the user's answer, for a `pair` started after it came.
    pending: Mutex<Option<(Device, String)>>,
    /// Devices found by mDNS, by service name: (id, name, platform).
    nearby: Mutex<HashMap<String, (String, String, String)>>,
    /// Items to put on the clipboard, in order.
    to_set: mpsc::Sender<Clip>,
    rt: tokio::runtime::Handle,
}

impl App {
    fn node(&self) -> &Arc<Node> {
        self.node.get().expect("node started")
    }

    /// Answers the pending code for `id`, once: the user, or the timeout of that `code` (not of a newer one).
    fn answer(&self, id: &str, code: Option<&str>, accept: bool) {
        let mut pending = self.pending.lock().unwrap();
        if pending
            .as_ref()
            .is_some_and(|(d, c)| d.id == id && code.is_none_or(|code| code == c))
        {
            *pending = None;
            self.node().confirm_pairing(id.into(), accept);
        }
    }

    /// Devices found nearby that aren't paired, sorted so the numbers of `pair --nearby` stay put.
    fn nearby(&self) -> Vec<(String, String, String)> {
        let paired: HashSet<String> = self.node().devices().into_iter().map(|d| d.id).collect();
        let mut list: Vec<_> = self
            .nearby
            .lock()
            .unwrap()
            .values()
            .filter(|d| !paired.contains(&d.0))
            .cloned()
            .collect();
        list.sort_by(|a, b| (&a.1, &a.0).cmp(&(&b.1, &b.0)));
        list
    }
}

/// The core's callbacks, on its threads: hand the work off.
struct Hooks(Arc<App>);

impl Events for Hooks {
    fn on_clip(&self, clip: Clip) {
        let _ = self.0.to_set.send(clip);
    }

    fn on_offer(&self, offer: Offer) {
        let what = [offer.mimes.join(", "), offer.file_names.join(", ")].join(" ");
        let (from, what) = (clean(&offer.from), clean(&what));
        eprintln!("offer {}: {} bytes from {from}: {what}", offer.id, offer.size);
        let Some(node) = self.0.node.get().cloned().filter(|_| offer.size <= FETCH_LIMIT) else {
            return;
        };
        let to_set = self.0.to_set.clone();
        self.0.rt.spawn(async move {
            match node.fetch(offer.id).await {
                Ok(clip) => drop(to_set.send(clip)),
                Err(err) => eprintln!("fetch: {err}"),
            }
        });
    }

    fn on_devices(&self, devices: Vec<Device>) {
        let _ = self.0.events.send(Ev::Devices(devices));
    }

    fn on_pairing_code(&self, device: Device, code: String) {
        eprintln!(
            "pairing with {}: code {code}; confirm it in `sharedclip pair`",
            clean(&device.name)
        );
        *self.0.pending.lock().unwrap() = Some((device.clone(), code.clone()));
        let _ = self.0.events.send(Ev::Code(device.clone(), code.clone()));
        let app = self.0.clone();
        self.0.rt.spawn(async move {
            tokio::time::sleep(CODE_TIMEOUT).await;
            app.answer(&device.id, Some(&code), false);
        });
    }
}

pub fn run() {
    // A panic on any thread stops the daemon, so systemd restarts it instead of leaving it up but not syncing.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        std::process::exit(101);
    }));
    clip::use_trusted_socket();
    let watcher = match clip::watcher() {
        Ok(watcher) => watcher,
        Err(err) => {
            eprintln!(
                "sharedclip: can't watch the clipboard: {err}.\nThe compositor must offer ext-data-control-v1 or \
                 wlr-data-control: noobsbot-desktop (trusted socket), KDE, Sway, Hyprland, COSMIC and niri do; GNOME doesn't."
            );
            std::process::exit(1);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(err) = rt.block_on(serve(watcher)) {
        eprintln!("sharedclip: {err}");
        std::process::exit(1);
    }
}

async fn serve(watcher: wl_clipboard_rs::watch::Watcher) -> Result<(), String> {
    let sock = crate::socket_path();
    if UnixStream::connect(&sock).await.is_ok() {
        return Err(format!("already running ({})", sock.display()));
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;

    // Items from other devices go on the clipboard one at a time, in order.
    let (to_set, set_rx) = mpsc::channel::<Clip>();
    std::thread::spawn(move || {
        for clip in set_rx {
            if let Err(err) = clip::set(&clip.formats, &clip.files) {
                eprintln!("clipboard: {err}");
            }
        }
    });

    let data_dir = data_dir();
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_default();
    let app = Arc::new(App {
        node: OnceLock::new(),
        events: broadcast::channel(16).0,
        pending: Mutex::new(None),
        nearby: Mutex::new(HashMap::new()),
        to_set,
        rt: tokio::runtime::Handle::current(),
    });
    let config = Config {
        data_dir: data_dir.to_string_lossy().into_owned(),
        name: name.clone(),
        platform: Platform::Linux,
        port: 0,
    };
    let node = Node::start(config, Arc::new(Hooks(app.clone())))
        .await
        .map_err(|e| e.to_string())?;
    let _ = app.node.set(node.clone());
    // ponytail: never metered; read NetworkManager's `Metered` property later.
    node.set_metered(false);
    eprintln!("sharedclip: {name} ({}) on port {}", node.id(), node.port());

    // Local copies go to the node in order, from the watcher's thread.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (failed, mut watch_failed) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let err = clip::watch(watcher, |formats, files| drop(tx.send((formats, files))));
        let _ = failed.send(err.to_string());
    });
    let n = node.clone();
    tokio::spawn(async move {
        while let Some((formats, files)) = rx.recv().await {
            let n = n.clone();
            let mut copy = Box::pin(async move {
                if let Err(err) = n.copy(formats, files).await {
                    eprintln!("send: {err}");
                }
            });
            // `copy` waits until the devices have it: start it here, so it keeps its place in line, then let it
            // finish on its own while the next item goes.
            let started = std::future::poll_fn(|cx| std::task::Poll::Ready(copy.as_mut().poll(cx))).await;
            if started.is_pending() {
                tokio::spawn(copy);
            }
        }
    });

    let mdns = discovery(&app, &name).map_err(|e| format!("mDNS: {e}"))?;

    let a = app.clone();
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(err) => {
                    // Out of file descriptors, say: wait and try again.
                    eprintln!("control socket: {err}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let a = a.clone();
            tokio::spawn(async move {
                if let Err(err) = handle(&a, stream).await {
                    eprintln!("control socket: {err}");
                }
            });
        }
    });

    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let result = tokio::select! {
        _ = term.recv() => Ok(()),
        _ = tokio::signal::ctrl_c() => Ok(()),
        Ok(err) = &mut watch_failed => Err(format!("clipboard: {err}")),
    };
    // Say goodbye on mDNS, so other devices drop this one now.
    let goodbye = mdns.shutdown();
    node.stop().await;
    let _ = std::fs::remove_file(&sock);
    if let Ok(goodbye) = goodbye {
        let _ = goodbye.recv_timeout(Duration::from_secs(1));
    }
    result
}

/// `$XDG_DATA_HOME/sharedclip`, by default `~/.local/share/sharedclip`.
fn data_dir() -> PathBuf {
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share");
    std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(home, PathBuf::from)
        .join("sharedclip")
}

/// Publishes this device with mDNS and passes the devices found to the node. Returns the daemon, to stop it.
fn discovery(app: &Arc<App>, name: &str) -> Result<ServiceDaemon, mdns_sd::Error> {
    let (node, mdns) = (app.node(), ServiceDaemon::new()?);
    let id = node.id();
    // A unique instance and host name; the TXT record has the real name.
    let short = &id[..id.len().min(16)];
    let txt = [("id", id.as_str()), ("name", name), ("platform", "linux")];
    let me = ServiceInfo::new(
        SERVICE,
        short,
        &format!("sharedclip-{short}.local."),
        "",
        node.port(),
        &txt[..],
    )?;
    mdns.register(me.enable_addr_auto())?;
    let found = mdns.browse(SERVICE)?;
    let app = app.clone();
    std::thread::spawn(move || {
        while let Ok(event) = found.recv() {
            match event {
                ServiceEvent::ServiceResolved(s) => {
                    let Some(other) = s.get_property_val_str("id").filter(|other| *other != id) else {
                        continue;
                    };
                    let txt = |key| s.get_property_val_str(key).unwrap_or_default().to_owned();
                    let device = (other.to_owned(), txt("name"), txt("platform"));
                    app.nearby.lock().unwrap().insert(s.fullname.clone(), device);
                    for ip in s.addresses.iter().map(|ip| ip.to_ip_addr()) {
                        // A link-local IPv6 address needs its interface, which "ip:port" can't carry.
                        if !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()) {
                            app.node().found(other.into(), SocketAddr::new(ip, s.port).to_string());
                        }
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => drop(app.nearby.lock().unwrap().remove(&fullname)),
                _ => {}
            }
        }
    });
    Ok(mdns)
}

/// Answers one CLI request.
async fn handle(app: &App, stream: UnixStream) -> std::io::Result<()> {
    let (read, mut out) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(request) = lines.next_line().await? else {
        return Ok(());
    };
    let node = app.node().clone();
    let (command, arg) = request.split_once(' ').unwrap_or((&request, ""));
    let text = match command {
        "pair" if arg.is_empty() => {
            let link = node.pairing_link();
            let qr = QrCode::new(&link).map(|q| {
                q.render::<Dense1x2>()
                    .dark_color(Dense1x2::Light)
                    .light_color(Dense1x2::Dark)
                    .build()
            });
            let qr = qr.unwrap_or_default();
            let text = format!(
                "Scan this, or run `sharedclip pair <link>` on the other device:\n{link}\n{qr}\n\
                 Waiting for a device (Ctrl-C to stop)...\n"
            );
            out.write_all(text.as_bytes()).await?;
            return session(app, lines, out, std::future::pending()).await;
        }
        "pair" if arg == "--nearby" => {
            let list = app.nearby();
            let rows = list
                .iter()
                .enumerate()
                .map(|(i, (_, name, platform))| format!("{}. {} ({})", i + 1, clean(name), clean(platform)));
            let rows = rows.collect::<Vec<_>>().join("\n");
            if rows.is_empty() {
                "No devices found nearby.".into()
            } else {
                format!("{rows}\nPair with one: sharedclip pair --nearby <n>")
            }
        }
        "pair" if arg.starts_with("--nearby ") => {
            let n: usize = arg["--nearby ".len()..].trim().parse().unwrap_or(0);
            let Some((id, ..)) = n.checked_sub(1).and_then(|i| app.nearby().into_iter().nth(i)) else {
                return out
                    .write_all(b"error: no such device; see sharedclip pair --nearby\n")
                    .await;
            };
            return session(app, lines, out, async move { node.pair_device(id).await }).await;
        }
        "pair" => match node.pair_link(arg.into()).await {
            Ok(device) => format!("Paired with {}", clean(&device.name)),
            Err(err) => format!("error: {err}"),
        },
        "devices" => {
            let rows = node.devices().into_iter().map(|d| {
                let state = if d.online { "online" } else { "offline" };
                format!("{}  ({:?}, {state})  {}", clean(&d.name), d.platform, clean(&d.id))
            });
            let rows = rows.collect::<Vec<_>>().join("\n");
            if rows.is_empty() {
                "No paired devices. Pair one with: sharedclip pair".into()
            } else {
                rows
            }
        }
        "unpair" => {
            let found: Vec<Device> = node
                .devices()
                .into_iter()
                .filter(|d| d.id == arg || d.name.eq_ignore_ascii_case(arg))
                .collect();
            match &found[..] {
                [device] => match node.unpair(device.id.clone()).await {
                    Ok(()) => format!("Unpaired {}", clean(&device.name)),
                    Err(err) => format!("error: {err}"),
                },
                [] => format!("error: no paired device {arg:?}; see sharedclip devices"),
                _ => format!("error: several devices are named {arg:?}; use the id from sharedclip devices"),
            }
        }
        "history" => {
            let names: HashMap<String, String> = node.devices().into_iter().map(|d| (d.id, d.name)).collect();
            let rows = node
                .history()
                .into_iter()
                .map(|c| summary(&c, names.get(&c.from).map_or("this device", |n| n.as_str())));
            let rows = rows.collect::<Vec<_>>().join("\n");
            if rows.is_empty() { "Nothing yet.".into() } else { rows }
        }
        "pause" => {
            node.set_paused(true);
            "Paused: nothing is sent or received until sharedclip resume.".into()
        }
        "resume" => {
            node.set_paused(false);
            "Resumed.".into()
        }
        _ => format!("error: unknown request {request:?}"),
    };
    out.write_all(format!("{text}\n").as_bytes()).await
}

/// Forwards pairing codes to the CLI and its answers back (`confirm <id> <prompt>`, answered `y <id>` or `n <id>`)
/// until a device pairs, `done` ends or the CLI leaves.
async fn session(
    app: &App,
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut out: OwnedWriteHalf,
    done: impl Future<Output = Result<Device, Error>>,
) -> std::io::Result<()> {
    let mut events = app.events.subscribe();
    let mut known: HashSet<String> = app.node().devices().into_iter().map(|d| d.id).collect();
    let confirm = |d: &Device, code: &str| {
        format!(
            "confirm {} Pairing code with {}: {code}. Accept? [y/N] \n",
            clean(&d.id),
            clean(&d.name)
        )
    };
    let pending = app.pending.lock().unwrap().clone();
    if let Some((device, code)) = pending {
        out.write_all(confirm(&device, &code).as_bytes()).await?;
    }
    tokio::pin!(done);
    loop {
        tokio::select! {
            result = &mut done => {
                let text = match result {
                    Ok(d) if known.insert(d.id.clone()) => format!("Paired with {}\n", clean(&d.name)),
                    Ok(_) => String::new(),
                    Err(err) => format!("error: {err}\n"),
                };
                return out.write_all(text.as_bytes()).await;
            }
            event = events.recv() => match event {
                Ok(Ev::Code(device, code)) => out.write_all(confirm(&device, &code).as_bytes()).await?,
                Ok(Ev::Devices(devices)) => {
                    let new: Vec<String> = devices.into_iter().filter(|d| known.insert(d.id.clone())).map(|d| clean(&d.name)).collect();
                    if !new.is_empty() {
                        return out.write_all(format!("Paired with {}\n", new.join(", ")).as_bytes()).await;
                    }
                }
                Err(_) => {} // lagged: missed some events
            },
            answer = lines.next_line() => match answer?.as_deref().and_then(|a| a.split_once(' ')) {
                Some((yes, id)) => app.answer(id, None, yes == "y"),
                None => return Ok(()), // the CLI left (Ctrl-C)
            },
        }
    }
}

/// One history line: age, where it came from and what it is.
fn summary(clip: &Summary, from: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mins = now.saturating_sub(clip.time_ms) / 60_000;
    let age = match mins {
        0..60 => format!("{mins}m"),
        60..1440 => format!("{}h", mins / 60),
        _ => format!("{}d", mins / 1440),
    };
    let what = match &clip.text {
        Some(text) => text.lines().next().unwrap_or_default().chars().take(60).collect(),
        None if !clip.files.is_empty() => clip
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        None => clip.mimes.join(", "),
    };
    format!("{age:>4}  {:<20}  {}", clean(from), clean(&what))
}

/// `s` with control characters (newlines, terminal escapes) replaced: names and text from other devices go to the
/// terminal and the control socket's lines.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { char::REPLACEMENT_CHARACTER } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn clean_strips_control_characters() {
        assert_eq!(
            super::clean("a\nconfirm x\x1b[2Jb\u{9b}"),
            "a\u{fffd}confirm x\u{fffd}[2Jb\u{fffd}"
        );
        assert_eq!(super::clean("Anil's phone é"), "Anil's phone é");
    }
}
