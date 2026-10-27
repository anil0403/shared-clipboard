//! The Wayland clipboard through data-control (wl-clipboard-rs): watch it, set it, and map its types to the core's
//! `Format`s and `File`s.

use std::env;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use sharedclip::{File, Format};
use wl_clipboard_rs::copy::{self, MimeSource, MimeType, Options, Source};
use wl_clipboard_rs::paste::{self, Seat};
use wl_clipboard_rs::watch::{ClipboardEvent, ClipboardType, Watcher};

/// Password managers (KeePassXC, KDE's) offer this type, set to `secret`, with a password.
const SECRET: &str = "x-kde-passwordManagerHint";
/// The text types we offer for a text item.
const TEXT: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
// ponytail: one size and time limit for every type; a bigger image or a slow app is skipped, not offered.
const MAX_READ: u64 = 100 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Hash of the item we last put on the clipboard or sent: our own changes and the compositor's re-offer of its kept
/// copy (when the app that copied quits) come back with the same content and are skipped.
static LAST: Mutex<u64> = Mutex::new(0);

/// Uses `$WAYLAND_DISPLAY-trusted` when it exists: noobsbot-desktop offers data-control only on that socket.
pub fn use_trusted_socket() {
    let (Some(dir), Some(mut display)) = (env::var_os("XDG_RUNTIME_DIR"), env::var_os("WAYLAND_DISPLAY")) else {
        return;
    };
    display.push("-trusted");
    if Path::new(&dir).join(&display).exists() {
        // SAFETY: called before any other thread starts.
        unsafe { env::set_var("WAYLAND_DISPLAY", display) };
    }
}

/// Starts watching; fails when the compositor offers no data-control.
pub fn watcher() -> Result<Watcher, paste::Error> {
    Watcher::new(ClipboardType::Regular, Seat::Unspecified)
}

/// Calls `send` with each new local item: not a password, not ours, not a repeat. Blocks; returns on a Wayland error.
pub fn watch(mut watcher: Watcher, send: impl Fn(Vec<Format>, Vec<File>)) -> paste::Error {
    // The first event is what was on the clipboard before we started: remembered, not sent.
    let mut first = true;
    loop {
        let event = match watcher.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => continue, // cancelled; we never cancel
            Err(err) => return err,
        };
        let first = std::mem::take(&mut first);
        let ClipboardEvent::Changed {
            mime_types, mut offer, ..
        } = event
        else {
            continue;
        };
        if mime_types.iter().any(|m| m == SECRET) {
            continue;
        }
        let mut read = |mime: &str| {
            let data = offer.receive(mime).ok().and_then(read_pipe);
            if data.is_none() {
                eprintln!("clipboard: skipped {mime}: unreadable, over 100 MB or slower than 10 s");
            }
            data.filter(|d| !d.is_empty())
        };
        let formats: Vec<Format> = pick(&mime_types)
            .into_iter()
            .filter_map(|(from, to)| {
                Some(Format {
                    mime: to.into(),
                    data: read(from)?,
                })
            })
            .collect();
        let mut files = Vec::new();
        if mime_types.iter().any(|m| m == "text/uri-list") {
            files = read("text/uri-list")
                .map(|d| parse_uri_list(&String::from_utf8_lossy(&d)))
                .unwrap_or_default();
        }
        if is_new(hash(&formats, &files)) && !first && !(formats.is_empty() && files.is_empty()) {
            send(formats, files);
        }
    }
}

/// Puts an item from another device on the clipboard, with every format, and files as `text/uri-list` and
/// `x-special/gnome-copied-files`. Remembers it, so its change event isn't sent back.
pub fn set(formats: &[Format], files: &[File]) -> Result<(), copy::Error> {
    let sources = offer(formats, files);
    is_new(hash(&read_back(&sources), files));
    let sources = sources
        .into_iter()
        .map(|(mime, data)| MimeSource {
            source: Source::Bytes(data.into()),
            mime_type: MimeType::Specific(mime),
        })
        .collect();
    let mut options = Options::new();
    options.omit_additional_text_mime_types(true);
    options.copy_multi(sources)
}

/// The (type, data) pairs we offer for an item; the first of a type wins, as in wl-clipboard-rs.
fn offer(formats: &[Format], files: &[File]) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |mime: &str, data: &[u8]| {
        if !out.iter().any(|(m, _)| m == mime) {
            out.push((mime.into(), data.into()));
        }
    };
    for f in formats {
        if f.mime.starts_with("text/plain") {
            TEXT.iter().for_each(|m| add(m, &f.data));
        } else {
            add(&f.mime, &f.data);
        }
    }
    if !files.is_empty() {
        let uris = uris(files);
        add(
            "text/uri-list",
            uris.iter().map(|u| format!("{u}\r\n")).collect::<String>().as_bytes(),
        );
        add(
            "x-special/gnome-copied-files",
            format!("copy\n{}", uris.join("\n")).as_bytes(),
        );
    }
    out
}

fn data<'a>(sources: &'a [(String, Vec<u8>)], mime: &str) -> &'a [u8] {
    sources.iter().find(|(m, _)| m == mime).map_or(&[], |(_, d)| d)
}

/// The formats the watcher reads back from what we offer.
fn read_back(sources: &[(String, Vec<u8>)]) -> Vec<Format> {
    let mimes: Vec<String> = sources.iter().map(|(m, _)| m.clone()).collect();
    pick(&mimes)
        .into_iter()
        .map(|(from, to)| Format {
            mime: to.into(),
            data: data(sources, from).to_vec(),
        })
        .collect()
}

/// The offered types we send, as (type offered, type sent): text, HTML and PNG. Files come from `text/uri-list`.
fn pick(offered: &[String]) -> Vec<(&'static str, &'static str)> {
    let has = |m: &&str| offered.iter().any(|o| o == m);
    let text = ["text/plain;charset=utf-8", "text/plain"]
        .into_iter()
        .find(has)
        .map(|m| (m, "text/plain"));
    text.into_iter()
        .chain(["text/html", "image/png"].into_iter().filter(has).map(|m| (m, m)))
        .collect()
}

/// The local files in a `text/uri-list`. Folders, other hosts, other schemes and non-UTF-8 paths are left out.
fn parse_uri_list(list: &str) -> Vec<File> {
    list.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("file://")?;
            // `file:///a` or `file://localhost/a`; `file://otherhost/a` is someone else's.
            let path = unescape(rest.strip_prefix("localhost").unwrap_or(rest)).filter(|p| p.starts_with('/'))?;
            let size = std::fs::metadata(&path).ok().filter(|m| m.is_file())?.len();
            let name = Path::new(&path).file_name()?.to_str()?.to_owned();
            Some(File { name, path, size })
        })
        .collect()
}

/// `file://` URIs for the files' paths.
fn uris(files: &[File]) -> Vec<String> {
    let escape = |b: u8| {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            (b as char).to_string()
        } else {
            format!("%{b:02X}")
        }
    };
    files
        .iter()
        .map(|f| format!("file://{}", f.path.bytes().map(escape).collect::<String>()))
        .collect()
}

fn unescape(s: &str) -> Option<String> {
    let (bytes, mut out, mut i) = (s.as_bytes(), Vec::new(), 0);
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Reads one type's data on its own thread, so an app that never answers can't hang the watcher.
fn read_pipe(pipe: impl Read + Send + 'static) -> Option<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let ok = pipe.take(MAX_READ + 1).read_to_end(&mut data).is_ok() && data.len() as u64 <= MAX_READ;
        let _ = tx.send(ok.then_some(data));
    });
    rx.recv_timeout(READ_TIMEOUT).ok().flatten()
}

fn hash(formats: &[Format], files: &[File]) -> u64 {
    let mut h = DefaultHasher::new();
    for f in formats {
        (&f.mime, &f.data).hash(&mut h);
    }
    for f in files {
        f.path.hash(&mut h);
    }
    h.finish()
}

/// Remembers `hash` as the last item; false if it already was.
fn is_new(hash: u64) -> bool {
    std::mem::replace(&mut *LAST.lock().unwrap(), hash) != hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(mime: &str, data: &str) -> Format {
        Format {
            mime: mime.into(),
            data: data.into(),
        }
    }

    #[test]
    fn picks_text_html_png() {
        let offered = |m: &[&str]| pick(&m.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            offered(&["text/plain", "text/plain;charset=utf-8"]),
            [("text/plain;charset=utf-8", "text/plain")]
        );
        assert_eq!(offered(&["STRING", "text/plain"]), [("text/plain", "text/plain")]);
        assert_eq!(
            offered(&[
                "image/jpeg",
                "image/png",
                "text/html",
                "application/x-qt-image",
                "text/uri-list"
            ]),
            [("text/html", "text/html"), ("image/png", "image/png")]
        );
        assert!(offered(&["UTF8_STRING", "image/bmp"]).is_empty());
    }

    #[test]
    fn uri_list_round_trip() {
        let dir = env::temp_dir().join(format!("sharedclip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("my file #1 é.txt");
        std::fs::write(&path, "hello").unwrap();
        let path = path.to_str().unwrap().to_owned();
        let file = File {
            name: "my file #1 é.txt".into(),
            path: path.clone(),
            size: 5,
        };

        let uri = &uris(std::slice::from_ref(&file))[0];
        assert!(
            uri.starts_with("file:///") && uri.ends_with("my%20file%20%231%20%C3%A9.txt"),
            "{uri}"
        );
        let list = format!(
            "# a comment\r\n{uri}\r\nhttps://example.com/x\r\nfile://localhost{}\r\n",
            &uri[7..]
        );
        let dir_uri = format!("file://{}", dir.to_str().unwrap());
        let parsed = parse_uri_list(&format!("{list}{dir_uri}\nfile://otherhost/etc/hostname\nfile:///nope"));
        assert_eq!(
            parsed,
            [file.clone(), file.clone()],
            "folders, other hosts and missing files are left out"
        );

        // What we offer for files parses back to the same files.
        let offered = offer(&[], std::slice::from_ref(&file));
        assert_eq!(
            parse_uri_list(&String::from_utf8_lossy(data(&offered, "text/uri-list"))),
            [file]
        );
        assert_eq!(
            String::from_utf8_lossy(data(&offered, "x-special/gnome-copied-files")),
            format!("copy\n{uri}")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repeats_are_skipped() {
        let a = hash(&[fmt("text/plain", "a")], &[]);
        let b = hash(&[fmt("text/plain", "b")], &[]);
        assert!(is_new(a));
        assert!(!is_new(a), "the same item again");
        assert!(is_new(b));
        assert!(is_new(a), "a changed back");

        // What we set from another device reads back with the same hash, so its change event isn't sent back.
        let incoming = [
            fmt("text/plain;charset=utf-8", "hi"),
            fmt("text/html", "<b>hi</b>"),
            fmt("image/jpeg", "?"),
        ];
        let offered = offer(&incoming, &[]);
        let mimes: Vec<&str> = offered.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            mimes,
            [
                "text/plain;charset=utf-8",
                "text/plain",
                "UTF8_STRING",
                "STRING",
                "TEXT",
                "text/html",
                "image/jpeg"
            ]
        );
        assert_eq!(
            read_back(&offered),
            [fmt("text/plain", "hi"), fmt("text/html", "<b>hi</b>")]
        );
    }
}
