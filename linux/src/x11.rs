//! The X11 clipboard: the CLIPBOARD selection as the ICCCM has it, with XFixes to hear about changes. Each side has its
//! own connection, so what we own is still served while the watcher waits on a slow app.

use std::collections::HashMap;
use std::error::Error;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use sharedclip::{File, Format};
use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask, SelectionNotifyEvent as Changed};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE};

use crate::clip::{MAX_READ, READ_TIMEOUT, SECRET, TEXT, changed, is_new, offer, set_hash};

/// The types the watcher looks for. `pick` knows text by its MIME types; see `TEXT_NAMES`.
const WANTED: [&str; 8] = [
    SECRET,
    "text/uri-list",
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "text/html",
    "image/png",
];
/// Older apps offer text only under X's names: UTF-8 and Latin-1.
const TEXT_NAMES: [(&str, &str); 2] = [("text/plain;charset=utf-8", "UTF8_STRING"), ("text/plain", "STRING")];
/// Bigger items go by INCR, in pieces of this size. Well under any server's largest request.
const CHUNK: usize = 64 * 1024;
/// An INCR transfer with no progress for this long is dropped, with its hold on the data.
const STALLED: Duration = Duration::from_secs(30);

static OWNER: OnceLock<Arc<Owner>> = OnceLock::new();

fn atom(conn: &RustConnection, name: &str) -> Result<Atom, ReplyError> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// An unmapped window that hears about its properties.
fn window(conn: &RustConnection, root: Window) -> Result<Window, ReplyOrIdError> {
    let win = conn.generate_id()?;
    let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
    let class = WindowClass::INPUT_ONLY;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        class,
        COPY_FROM_PARENT,
        &aux,
    )?;
    Ok(win)
}

/// Text from X's `STRING` type, as UTF-8.
fn from_latin1(data: &[u8]) -> Vec<u8> {
    data.iter().map(|&b| b as char).collect::<String>().into_bytes()
}

/// UTF-8 text for X's `STRING` type.
fn to_latin1(text: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(text);
    text.chars().map(|c| u8::try_from(c).unwrap_or(b'?')).collect()
}

/// The watching side.
pub struct Watcher {
    conn: Arc<RustConnection>,
    root: Window,
    clipboard: Atom,
    targets: Atom,
    incr: Atom,
    property: Atom,
    wanted: Vec<(&'static str, Atom)>,
    /// The owner's window: its changes are `set`'s, and aren't read back.
    own: Window,
    events: Receiver<Result<Event, String>>,
    /// The latest change that came during a read.
    again: Option<Changed>,
    lost: Option<String>,
}

/// True once `watcher` has connected to an X server.
pub fn running() -> bool {
    OWNER.get().is_some()
}

/// Connects the watcher and the owner, which starts serving.
pub fn watcher() -> Result<Watcher, String> {
    start().map_err(|err| format!("X server: {err}"))
}

fn start() -> Result<Watcher, Box<dyn Error>> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    conn.xfixes_query_version(5, 0)?.reply()?;
    let clipboard = atom(&conn, "CLIPBOARD")?;
    let win = window(&conn, root)?;
    conn.xfixes_select_selection_input(win, clipboard, SelectionEventMask::SET_SELECTION_OWNER)?;
    conn.flush()?;
    let (targets, incr, property) = (
        atom(&conn, "TARGETS")?,
        atom(&conn, "INCR")?,
        atom(&conn, "SHAREDCLIP")?,
    );
    let wanted = WANTED
        .iter()
        .map(|&name| Ok((name, atom(&conn, name)?)))
        .collect::<Result<_, ReplyError>>()?;

    let owner = Arc::new(Owner::new()?);
    let own = owner.win;
    OWNER.set(owner.clone()).map_err(|_| "connected twice")?;
    let (tx, events) = mpsc::channel();
    let failed = tx.clone();
    thread::spawn(move || {
        if let Err(err) = owner.serve() {
            let _ = failed.send(Err(err.to_string()));
        }
    });
    let conn = Arc::new(conn);
    let c = conn.clone();
    thread::spawn(move || {
        loop {
            match c.wait_for_event() {
                Ok(event) => {
                    if tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
                Err(err) => return drop(tx.send(Err(err.to_string()))),
            }
        }
    });
    Ok(Watcher {
        conn,
        root,
        clipboard,
        targets,
        incr,
        property,
        wanted,
        own,
        events,
        again: None,
        lost: None,
    })
}

/// Calls `send` with each new local item. Blocks; returns the error that lost the X server.
pub fn watch(mut w: Watcher, send: impl Fn(Vec<Format>, Vec<File>)) -> String {
    loop {
        if let Some(err) = w.lost.take() {
            return err;
        }
        let event = match w.again.take() {
            Some(event) => event,
            None => match w.events.recv() {
                Ok(Ok(Event::XfixesSelectionNotify(event))) => event,
                Ok(Ok(_)) => continue,
                Ok(Err(err)) => return err,
                Err(_) => return "lost the X server".into(),
            },
        };
        // A clipboard manager taking over from an app that quit comes here too: it reads as a repeat.
        if event.owner == NONE || event.owner == w.own {
            continue;
        }
        let Some(types) = w.offered(event.timestamp) else {
            continue;
        };
        let mimes: Vec<String> = types.iter().map(|(mime, _)| mime.to_string()).collect();
        let read = |mime: &str| {
            let &(_, target) = types.iter().find(|(m, _)| *m == mime)?;
            let data = w.fetch(target, event.timestamp)?;
            Some(match target == Atom::from(AtomEnum::STRING) {
                true => from_latin1(&data),
                false => data,
            })
        };
        changed(&mimes, read, false, &send);
    }
}

impl Watcher {
    /// The wanted types on offer, as (type for `changed`, target to ask for).
    fn offered(&mut self, time: Timestamp) -> Option<Vec<(&'static str, Atom)>> {
        let targets = self.fetch(self.targets, time)?;
        let targets: Vec<Atom> = targets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&b| Atom::from_ne_bytes(b))
            .collect();
        let mut found: Vec<_> = self
            .wanted
            .iter()
            .filter(|(_, a)| targets.contains(a))
            .copied()
            .collect();
        for (mime, name) in TEXT_NAMES {
            let has = |m: &str| found.iter().find(|(n, _)| *n == m).copied();
            if let (None, Some((_, target))) = (has(mime), has(name)) {
                found.push((mime, target));
            }
        }
        Some(found)
    }

    /// One type of what's on the clipboard. Each read has its own window: a late answer to one that gave up is lost
    /// with its window, and can't be mistaken for the next.
    fn fetch(&mut self, target: Atom, time: Timestamp) -> Option<Vec<u8>> {
        let win = window(&self.conn, self.root).ok()?;
        let data = self.receive(win, target, time);
        let _ = self.conn.destroy_window(win);
        let _ = self.conn.flush();
        data
    }

    fn receive(&mut self, win: Window, target: Atom, time: Timestamp) -> Option<Vec<u8>> {
        let deadline = Instant::now() + READ_TIMEOUT;
        let conn = self.conn.clone();
        let property = self.property;
        conn.convert_selection(win, self.clipboard, target, property, time)
            .ok()?;
        conn.flush().ok()?;
        loop {
            match self.next(deadline)? {
                Event::SelectionNotify(e) if e.requestor == win && e.property == NONE => return None,
                Event::SelectionNotify(e) if e.requestor == win => break,
                _ => {}
            }
        }
        // Deleting the property is what asks an INCR sender for the next piece.
        let take = || {
            let cookie = conn.get_property(true, win, property, AtomEnum::ANY, 0, (MAX_READ / 4 + 1) as u32);
            Some(cookie.ok()?.reply().ok()?).filter(|r| r.bytes_after == 0)
        };
        let reply = take()?;
        if reply.type_ != self.incr {
            return Some(reply.value);
        }
        let mut data = Vec::new();
        loop {
            match self.next(deadline)? {
                Event::PropertyNotify(e) if e.window == win && e.state == Property::NEW_VALUE => {
                    let piece = take()?.value;
                    if piece.is_empty() {
                        return Some(data);
                    }
                    data.extend_from_slice(&piece);
                    if data.len() as u64 > MAX_READ {
                        return None;
                    }
                }
                _ => {}
            }
        }
    }

    /// The next event for a read, or None at the deadline. A change of the clipboard is kept for afterwards.
    fn next(&mut self, deadline: Instant) -> Option<Event> {
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(wait).ok()? {
                Ok(Event::XfixesSelectionNotify(event)) => self.again = Some(event),
                Ok(event) => return Some(event),
                Err(err) => {
                    self.lost = Some(err);
                    return None;
                }
            }
        }
    }
}

/// Puts an item from another device on the clipboard. Marks it as the last item, as `watch` doesn't read it back.
pub fn set(formats: &[Format], files: &[File]) -> Result<(), String> {
    let owner = OWNER.get().ok_or("not connected to the X server")?;
    let sources = offer(formats, files);
    if sources.is_empty() {
        return Err("nothing to offer".into());
    }
    is_new(set_hash(&sources, files));
    let utf8 = atom(&owner.conn, "UTF8_STRING").map_err(|e| e.to_string())?;
    let mut text = None;
    let mut offers = Vec::new();
    for (mime, data) in sources {
        let target = atom(&owner.conn, &mime).map_err(|e| e.to_string())?;
        let (kind, data) = match mime.as_str() {
            "STRING" => (target, Arc::new(to_latin1(&data))),
            // One copy for every text type. TEXT goes out as a type of the owner's choosing.
            m if TEXT.contains(&m) => {
                let kind = if m == "TEXT" { utf8 } else { target };
                (kind, text.get_or_insert_with(|| Arc::new(data)).clone())
            }
            _ => (target, Arc::new(data)),
        };
        offers.push(Offer { target, kind, data });
    }
    *owner.pending.lock().unwrap() = Some(offers);
    owner.poke().map_err(|e| e.to_string())
}

/// A type of the item we own, with the property type it goes out as.
struct Offer {
    target: Atom,
    kind: Atom,
    data: Arc<Vec<u8>>,
}

/// A big item going to one requestor, piece by piece, as it deletes each.
struct Transfer {
    kind: Atom,
    data: Arc<Vec<u8>>,
    sent: usize,
    last: Instant,
}

/// The owning side. Only `serve`, on its thread, touches the selection.
struct Owner {
    conn: RustConnection,
    win: Window,
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    poke: Atom,
    /// What `set` left for `serve`; a newer item replaces one not yet taken.
    pending: Mutex<Option<Vec<Offer>>>,
}

impl Owner {
    fn new() -> Result<Self, Box<dyn Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let win = window(&conn, root)?;
        conn.flush()?;
        Ok(Self {
            win,
            clipboard: atom(&conn, "CLIPBOARD")?,
            targets: atom(&conn, "TARGETS")?,
            timestamp: atom(&conn, "TIMESTAMP")?,
            incr: atom(&conn, "INCR")?,
            poke: atom(&conn, "SHAREDCLIP_SET")?,
            pending: Mutex::new(None),
            conn,
        })
    }

    /// Wakes `serve` with a change to our window. The event brings the server's time, which claiming needs.
    fn poke(&self) -> Result<(), ConnectionError> {
        let (win, poke) = (self.win, self.poke);
        self.conn
            .change_property8(PropMode::APPEND, win, poke, AtomEnum::STRING, &[])?;
        self.conn.flush()
    }

    /// Claims the clipboard for what `set` leaves and answers apps. Returns when the connection is lost; an error from
    /// a single app, like a window closing mid-transfer, comes as an event and is ignored.
    fn serve(&self) -> Result<(), ReplyError> {
        let mut item: Option<Vec<Offer>> = None;
        let mut since = 0;
        let mut transfers: HashMap<(Window, Atom), Transfer> = HashMap::new();
        loop {
            match self.conn.wait_for_event()? {
                Event::PropertyNotify(e) if e.window == self.win => {
                    let offers = self.pending.lock().unwrap().take();
                    if let Some(offers) = offers {
                        self.conn.set_selection_owner(self.win, self.clipboard, e.time)?;
                        if self.owned()? {
                            (item, since) = (Some(offers), e.time);
                        } else {
                            eprintln!("clipboard: another app took the clipboard first");
                        }
                    }
                }
                Event::SelectionRequest(e) => {
                    let offers = item.as_deref().filter(|_| e.selection == self.clipboard);
                    self.answer(&e, offers, since, &mut transfers)?;
                }
                Event::PropertyNotify(e) if e.state == Property::DELETE => self.next_piece(&e, &mut transfers)?,
                // We may have claimed it again since that app did.
                Event::SelectionClear(_) if !self.owned()? => item = None,
                _ => {}
            }
            transfers.retain(|_, t| t.last.elapsed() < STALLED);
            self.conn.flush()?;
        }
    }

    fn owned(&self) -> Result<bool, ReplyError> {
        let reply = self.conn.get_selection_owner(self.clipboard)?.reply()?;
        Ok(reply.owner == self.win)
    }

    fn answer(
        &self,
        e: &SelectionRequestEvent,
        offers: Option<&[Offer]>,
        since: Timestamp,
        transfers: &mut HashMap<(Window, Atom), Transfer>,
    ) -> Result<(), ConnectionError> {
        // Clients from before ICCCM 2 leave the property out.
        let property = if e.property == NONE { e.target } else { e.property };
        // A requestor reusing a property has given up on any transfer there.
        transfers.remove(&(e.requestor, property));
        let conn = &self.conn;
        // ponytail: MULTIPLE is refused, which ICCCM doesn't allow, but no toolkit asks for it. Add it if one does.
        let stored = match offers {
            None => false,
            Some(offers) if e.target == self.targets => {
                let list: Vec<Atom> = [self.targets, self.timestamp]
                    .into_iter()
                    .chain(offers.iter().map(|o| o.target))
                    .collect();
                conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &list)?;
                true
            }
            Some(_) if e.target == self.timestamp => {
                conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::INTEGER, &[since])?;
                true
            }
            Some(offers) => match offers.iter().find(|o| o.target == e.target) {
                None => false,
                Some(o) if o.data.len() <= CHUNK => {
                    conn.change_property8(PropMode::REPLACE, e.requestor, property, o.kind, &o.data)?;
                    true
                }
                Some(o) => {
                    // The requestor's deletions must reach us before it can hear about the transfer.
                    let aux = ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);
                    conn.change_window_attributes(e.requestor, &aux)?;
                    let size = [o.data.len() as u32];
                    conn.change_property32(PropMode::REPLACE, e.requestor, property, self.incr, &size)?;
                    let transfer = Transfer {
                        kind: o.kind,
                        data: o.data.clone(),
                        sent: 0,
                        last: Instant::now(),
                    };
                    transfers.insert((e.requestor, property), transfer);
                    true
                }
            },
        };
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if stored { property } else { NONE },
        };
        conn.send_event(false, e.requestor, EventMask::NO_EVENT, event)?;
        Ok(())
    }

    /// The requestor took a piece: sends the next, or the empty one that ends the transfer.
    fn next_piece(
        &self,
        e: &PropertyNotifyEvent,
        transfers: &mut HashMap<(Window, Atom), Transfer>,
    ) -> Result<(), ConnectionError> {
        let Some(t) = transfers.get_mut(&(e.window, e.atom)) else {
            return Ok(());
        };
        let end = (t.sent + CHUNK).min(t.data.len());
        let piece = &t.data[t.sent..end];
        self.conn
            .change_property8(PropMode::REPLACE, e.window, e.atom, t.kind, piece)?;
        if piece.is_empty() {
            transfers.remove(&(e.window, e.atom));
            if !transfers.keys().any(|(win, _)| *win == e.window) {
                let aux = ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT);
                self.conn.change_window_attributes(e.window, &aux)?;
            }
        } else {
            t.sent = end;
            t.last = Instant::now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin1_both_ways() {
        assert_eq!(from_latin1(b"caf\xe9"), "café".as_bytes());
        assert_eq!(to_latin1("café €".as_bytes()), b"caf\xe9 ?");
    }

    /// Against a real X server, with xclip installed: `xvfb-run -a cargo test -p sharedclip-linux x11 -- --ignored`.
    #[test]
    #[ignore = "needs an X server and xclip"]
    fn live_x11() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        std::env::var("DISPLAY").expect("DISPLAY");
        let fmt = |mime: &str, data: &[u8]| Format {
            mime: mime.into(),
            data: data.into(),
        };
        let paste = |target: &str| {
            let args = ["-selection", "clipboard", "-o", "-t", target];
            Command::new("xclip").args(args).output().unwrap().stdout
        };
        // xclip stays behind to serve pastes, so it mustn't hold our pipes.
        let copy = |target: &str, data: &[u8]| {
            let mut app = Command::new("xclip")
                .args(["-selection", "clipboard", "-i", "-t", target])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            app.stdin.take().unwrap().write_all(data).unwrap();
            app.wait().unwrap();
        };
        let (tx, rx) = mpsc::channel();
        let w = watcher().expect("X server");
        thread::spawn(move || watch(w, move |formats, files| tx.send((formats, files)).unwrap()));
        let next = || rx.recv_timeout(Duration::from_secs(3));

        // An app's copy is seen, under every name for text.
        copy("UTF8_STRING", "copied é".as_bytes());
        assert_eq!(next().unwrap().0, [fmt("text/plain", "copied é".as_bytes())]);
        copy("STRING", b"caf\xe9");
        assert_eq!(next().unwrap().0, [fmt("text/plain", "café".as_bytes())]);
        copy("text/plain;charset=utf-8", b"mime");
        assert_eq!(next().unwrap().0, [fmt("text/plain", b"mime")]);
        copy("text/html", b"<b>x</b>");
        assert_eq!(next().unwrap().0, [fmt("text/html", b"<b>x</b>")]);

        // Another owner with the same content, as a clipboard manager taking over: not sent twice.
        copy("text/html", b"<b>x</b>");
        assert!(next().is_err(), "a repeat isn't sent");

        // What we set is pasted in every type, and not sent back.
        let html = fmt("text/html", b"<b>from a phone</b>");
        set(&[fmt("text/plain", "from a phoné".as_bytes()), html], &[]).unwrap();
        thread::sleep(Duration::from_millis(300));
        for target in ["UTF8_STRING", "text/plain", "text/plain;charset=utf-8", "TEXT"] {
            assert_eq!(paste(target), "from a phoné".as_bytes(), "{target}");
        }
        assert_eq!(paste("STRING"), b"from a phon\xe9");
        assert_eq!(paste("text/html"), b"<b>from a phone</b>");
        let targets = String::from_utf8(paste("TARGETS")).unwrap();
        assert!(
            targets.contains("UTF8_STRING") && targets.contains("TIMESTAMP"),
            "{targets}"
        );
        assert_eq!(paste("x-special/gnome-copied-files"), b"", "a type we don't offer");
        assert!(next().is_err(), "our own items aren't sent back");

        // Big items go piece by piece, both ways.
        let big: Vec<u8> = (0..5_000_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        set(&[fmt("text/plain", &big)], &[]).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(paste("UTF8_STRING") == big, "INCR out");
        let bigger = [&big[..], b"!"].concat();
        copy("UTF8_STRING", &bigger);
        assert!(next().unwrap().0 == [fmt("text/plain", &bigger)], "INCR in");

        // Items in a burst leave the last on the clipboard, and none is sent back.
        for i in 0..200 {
            set(&[fmt("text/plain", format!("burst {i}").as_bytes())], &[]).unwrap();
        }
        thread::sleep(Duration::from_millis(300));
        assert_eq!(paste("UTF8_STRING"), b"burst 199");
        assert!(next().is_err(), "burst isn't sent back");

        // Copying back what came from another device is a repeat; something else is new.
        copy("UTF8_STRING", b"burst 199");
        assert!(next().is_err(), "the same item again");
        copy("UTF8_STRING", b"after the burst");
        assert_eq!(next().unwrap().0, [fmt("text/plain", b"after the burst")]);

        // Files: as `text/uri-list` for apps, and read back from one.
        let path = std::env::temp_dir().join(format!("sharedclip x11 {}.txt", std::process::id()));
        std::fs::write(&path, "file body").unwrap();
        let path = path.to_str().unwrap().to_owned();
        let file = File {
            name: "x".into(),
            path: path.clone(),
            size: 9,
        };
        set(&[], std::slice::from_ref(&file)).unwrap();
        thread::sleep(Duration::from_millis(300));
        let list = paste("text/uri-list");
        assert!(String::from_utf8_lossy(&list).starts_with("file:///"));
        assert_eq!(
            paste("x-special/gnome-copied-files"),
            [b"copy\n", &list[..list.len() - 2]].concat()
        );
        copy("UTF8_STRING", b"something else");
        next().unwrap();
        copy("text/uri-list", &list);
        let (formats, files) = next().unwrap();
        assert_eq!(
            (formats, files[0].path.as_str(), files[0].size),
            (vec![], path.as_str(), 9)
        );
        std::fs::remove_file(&path).unwrap();

        // Apps that misbehave, each through a small client of its own.
        let (app, screen) = x11rb::connect(None).unwrap();
        let root = app.setup().roots[screen].root;
        let clipboard = atom(&app, "CLIPBOARD").unwrap();
        let app_win = window(&app, root).unwrap();
        // A password manager's item is never sent, and neither is the secret read.
        app.set_selection_owner(app_win, clipboard, x11rb::CURRENT_TIME)
            .unwrap();
        app.flush().unwrap();
        let serve_once = |list: &[Atom]| loop {
            if let Event::SelectionRequest(e) = app.wait_for_event().unwrap() {
                app.change_property32(PropMode::REPLACE, e.requestor, e.property, AtomEnum::ATOM, list)
                    .unwrap();
                let event = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: e.time,
                    requestor: e.requestor,
                    selection: e.selection,
                    target: e.target,
                    property: e.property,
                };
                app.send_event(false, e.requestor, EventMask::NO_EVENT, event).unwrap();
                app.flush().unwrap();
                return e.target;
            }
        };
        let secret = atom(&app, SECRET).unwrap();
        let utf8 = atom(&app, "UTF8_STRING").unwrap();
        let targets_atom = atom(&app, "TARGETS").unwrap();
        assert_eq!(serve_once(&[secret, utf8]), targets_atom);
        assert!(next().is_err(), "a password isn't sent");

        // An app that never answers costs a timeout, and the clipboard is watched again after.
        let started = Instant::now();
        let other = window(&app, root).unwrap();
        app.set_selection_owner(other, clipboard, x11rb::CURRENT_TIME).unwrap();
        app.flush().unwrap();
        thread::sleep(Duration::from_millis(200));
        copy("UTF8_STRING", b"after a hung app");
        let got = rx.recv_timeout(READ_TIMEOUT * 2 + Duration::from_secs(3)).unwrap();
        assert_eq!(got.0, [fmt("text/plain", b"after a hung app")]);
        eprintln!("a hung app delayed the next copy by {:?}", started.elapsed());

        // A requestor that leaves in the middle of a transfer doesn't hurt what we serve.
        set(&[fmt("text/plain", &big)], &[]).unwrap();
        thread::sleep(Duration::from_millis(300));
        let (quitter, _) = x11rb::connect(None).unwrap();
        let win = window(&quitter, root).unwrap();
        let property = atom(&quitter, "QUITTER").unwrap();
        let utf8 = atom(&quitter, "UTF8_STRING").unwrap();
        quitter
            .convert_selection(win, clipboard, utf8, property, x11rb::CURRENT_TIME)
            .unwrap();
        quitter.flush().unwrap();
        loop {
            if let Event::SelectionNotify(_) = quitter.wait_for_event().unwrap() {
                break;
            }
        }
        quitter.delete_property(win, property).unwrap();
        quitter.flush().unwrap();
        drop(quitter);
        thread::sleep(Duration::from_millis(200));
        assert!(paste("UTF8_STRING") == big, "still served");
    }
}
