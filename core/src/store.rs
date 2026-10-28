//! What the core keeps in `data_dir`: the key, paired devices, history and received files.

use std::{fs, io::Write, path::Path};

use iroh::SecretKey;
use serde::{Deserialize, Serialize};

use crate::{
    Clip, Error, File, Format,
    protocol::{FormatPart, PeerInfo, valid_id},
};

/// How many items the history keeps.
pub const HISTORY_LEN: usize = 50;

pub fn io(e: impl std::fmt::Display) -> Error {
    Error::Io(e.to_string())
}

/// Loads this device's key from `data_dir/key`, or makes one.
// ponytail: a plain file (mode 0600); the key moves to the Keychain, Android Keystore or Secret Service later.
pub fn load_key(dir: &Path) -> Result<SecretKey, Error> {
    let path = dir.join("key");
    match fs::read(&path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes.try_into().map_err(|_| io("the key file is damaged"))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            opts.open(&path)
                .and_then(|mut f| f.write_all(&key.to_bytes()))
                .map_err(io)?;
            Ok(key)
        }
        Err(e) => Err(io(e)),
    }
}

pub fn load_peers(dir: &Path) -> Vec<PeerInfo> {
    read_json(&dir.join("peers.json"))
}

pub fn save_peers<'a>(dir: &Path, peers: impl Iterator<Item = &'a PeerInfo>) {
    write_json(&dir.join("peers.json"), &peers.collect::<Vec<_>>());
}

/// A history item in `history.json`; its formats' data is in `history/<id>`, one after another.
#[derive(Serialize, Deserialize)]
struct Stored {
    id: String,
    from: String,
    time_ms: u64,
    formats: Vec<FormatPart>,
    files: Vec<StoredFile>,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    name: String,
    path: String,
    size: u64,
}

pub fn load_history(dir: &Path) -> Vec<Clip> {
    let list: Vec<Stored> = read_json(&dir.join("history.json"));
    list.into_iter()
        .filter_map(|s| {
            if !valid_id(&s.id) {
                return None;
            }
            let blob = fs::read(dir.join("history").join(&s.id)).ok()?;
            let mut rest = blob.as_slice();
            let mut formats = Vec::new();
            for f in s.formats {
                let data = rest.split_off(..usize::try_from(f.size).ok()?)?;
                formats.push(Format {
                    mime: f.mime,
                    data: data.to_vec(),
                });
            }
            let files = s
                .files
                .into_iter()
                .map(|f| File {
                    name: f.name,
                    path: f.path,
                    size: f.size,
                })
                .collect();
            Some(Clip {
                id: s.id,
                from: s.from,
                time_ms: s.time_ms,
                formats,
                files,
            })
        })
        .take(HISTORY_LEN)
        .collect()
}

/// Saves the history list. The items' data must already be saved with `save_blob`.
pub fn save_history(dir: &Path, history: &[Clip]) {
    let list: Vec<Stored> = history
        .iter()
        .map(|c| Stored {
            id: c.id.clone(),
            from: c.from.clone(),
            time_ms: c.time_ms,
            formats: c
                .formats
                .iter()
                .map(|f| FormatPart {
                    mime: f.mime.clone(),
                    size: f.data.len() as u64,
                })
                .collect(),
            files: c
                .files
                .iter()
                .map(|f| StoredFile {
                    name: f.name.clone(),
                    path: f.path.clone(),
                    size: f.size,
                })
                .collect(),
        })
        .collect();
    write_json(&dir.join("history.json"), &list);
}

/// Saves an item's formats to `history/<id>`.
pub async fn save_blob(dir: &Path, clip: &Clip) -> Result<(), Error> {
    let blob = clip
        .formats
        .iter()
        .map(|f| f.data.as_slice())
        .collect::<Vec<_>>()
        .concat();
    let dir = dir.join("history");
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    tokio::fs::write(dir.join(&clip.id), blob).await.map_err(io)
}

/// Deletes an item that dropped out of the history: its data and any files we received for it.
pub fn forget(dir: &Path, id: &str) {
    let _ = fs::remove_file(dir.join("history").join(id));
    let _ = fs::remove_dir_all(dir.join("files").join(id));
}

/// A received file name that is safe to use as one path component.
pub fn safe_name(name: &str) -> String {
    let mut name: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    // File systems take 255 bytes; leave room for the prefix that tells clashing names apart.
    while name.len() > 200 {
        name.pop();
    }
    if name.is_empty() || name == "." || name == ".." {
        "file".into()
    } else {
        name
    }
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Writes through a temporary file, so a crash never leaves half a file.
fn write_json(path: &Path, value: &impl Serialize) {
    let tmp = path.with_extension("tmp");
    let ok = serde_json::to_vec(value)
        .ok()
        .is_some_and(|json| fs::write(&tmp, json).is_ok());
    if ok {
        let _ = fs::rename(tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("photo.png"), "photo.png");
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("a\\b"), "a_b");
        assert_eq!(safe_name("a\x1b[0m\n"), "a_[0m_");
        assert_eq!(safe_name(&"é".repeat(200)).len(), 200);
    }
}
