//! What the core keeps in `data_dir`: the key, paired devices, history and received files.

use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use iroh::SecretKey;
use serde::{Deserialize, Serialize};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    Clip, Error, File, Format, Summary,
    protocol::{FormatPart, MAX_FILE_NAME, PeerInfo, clean, valid_id},
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

/// Ids of devices the user unpaired.
pub fn load_removed(dir: &Path) -> HashSet<String> {
    read_json(&dir.join("removed.json"))
}

pub fn save_peers(dir: &Path, peers: &[PeerInfo], removed: &HashSet<String>) {
    write_json(&dir.join("peers.json"), &peers);
    write_json(&dir.join("removed.json"), removed);
}

/// A history item without its data, as `history.json` lists it. Its formats' data is in `history/<id>`, one after
/// another; its files are at their paths.
#[derive(Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub from: String,
    pub time_ms: u64,
    pub formats: Vec<FormatPart>,
    /// With the names as sent, which the item id covers.
    pub files: Vec<File>,
    /// The start of its plain text, for the history list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Meta {
    /// Total size of its formats and files.
    pub fn size(&self) -> u64 {
        let sizes = self
            .formats
            .iter()
            .map(|f| f.size)
            .chain(self.files.iter().map(|f| f.size));
        sizes.fold(0, u64::saturating_add)
    }

    /// Size of its formats' data in `history/<id>`.
    pub fn formats_size(&self) -> u64 {
        self.formats.iter().map(|f| f.size).fold(0, u64::saturating_add)
    }

    /// Text only: goes right away even to a metered device.
    pub fn is_text(&self) -> bool {
        self.files.is_empty() && self.formats.iter().all(|f| f.mime.starts_with("text/"))
    }

    pub fn summary(&self) -> Summary {
        Summary {
            id: self.id.clone(),
            from: self.from.clone(),
            time_ms: self.time_ms,
            mimes: self.formats.iter().map(|f| f.mime.clone()).collect(),
            size: self.size(),
            text: self.text.clone(),
            files: self.app_files(),
        }
    }

    /// Its files for the app, with names safe to show.
    pub fn app_files(&self) -> Vec<File> {
        let file = |f: &File| File {
            name: clean(&f.name, MAX_FILE_NAME),
            path: f.path.clone(),
            size: f.size,
        };
        self.files.iter().map(file).collect()
    }
}

/// How much of a text format we keep for `Meta::text`, in characters, and the bytes that always hold that many.
const PREVIEW_CHARS: usize = 300;
pub const PREVIEW_BYTES: usize = 4 * PREVIEW_CHARS + 3;

/// `Meta::text` from the first bytes of a `text/plain` format.
pub fn preview(head: &[u8]) -> String {
    String::from_utf8_lossy(head).chars().take(PREVIEW_CHARS).collect()
}

/// The history, newest first. Items whose data is missing are left out.
pub fn load_history(dir: &Path) -> Vec<Meta> {
    let list: Vec<Meta> = read_json(&dir.join("history.json"));
    let saved = |m: &Meta| fs::metadata(blob_path(dir, &m.id)).is_ok_and(|f| f.len() == m.formats_size());
    list.into_iter()
        .filter(|m| valid_id(&m.id) && saved(m))
        .take(HISTORY_LEN)
        .collect()
}

pub fn save_history(dir: &Path, history: &[Meta]) {
    write_json(&dir.join("history.json"), &history);
}

/// Where an item's formats' data is. `id` must be valid.
pub fn blob_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("history").join(id)
}

/// A new file to receive an item's formats into, in the folder of `blob_path`.
pub async fn blob_part(dir: &Path, id: &str) -> Result<PathBuf, Error> {
    let dir = dir.join("history");
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    Ok(dir.join(format!("{id}.{:x}.part", rand::random::<u64>())))
}

/// Saves an item's formats to `history/<id>`, through a part file, so a send reading the old copy isn't cut short.
pub async fn save_blob(dir: &Path, id: &str, formats: &[Format]) -> Result<(), Error> {
    let part = blob_part(dir, id).await?;
    let write = async {
        let mut out = tokio::fs::File::create(&part).await?;
        for f in formats {
            out.write_all(&f.data).await?;
        }
        out.flush().await?;
        tokio::fs::rename(&part, blob_path(dir, id)).await
    };
    let res = write.await.map_err(io);
    if res.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    res
}

/// Reads a whole item back from disk.
pub async fn load_item(dir: &Path, meta: &Meta) -> Result<Clip, Error> {
    let mut blob = tokio::fs::File::open(blob_path(dir, &meta.id)).await.map_err(io)?;
    let mut formats = Vec::new();
    for f in &meta.formats {
        let mut data = vec![0; usize::try_from(f.size).map_err(io)?];
        blob.read_exact(&mut data).await.map_err(io)?;
        formats.push(Format {
            mime: f.mime.clone(),
            data,
        });
    }
    Ok(Clip {
        id: meta.id.clone(),
        from: meta.from.clone(),
        time_ms: meta.time_ms,
        formats,
        files: meta.app_files(),
    })
}

/// Deletes an item that dropped out of the history: its data and any files we received for it. Can take a while.
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
