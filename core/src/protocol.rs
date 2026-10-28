//! The wire format from `docs/protocol.md`: messages, framing and item ids.

use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{Clip, Error, Platform};

pub const ALPN: &[u8] = b"sharedclip/1";
pub const PAIR_ALPN: &[u8] = b"sharedclip/pair/1";
/// Largest message header we read.
pub const MAX_HEADER: usize = 64 * 1024;
/// Largest pairing message we read. `paired` lists every other device, with up to 8 addresses each.
pub const MAX_PAIR_MSG: usize = 64 * 1024;
/// A `clip` body may be this much over `SIZE_CAP` before we refuse it.
pub const BODY_SLACK: u64 = 1024 * 1024;

/// A message on `sharedclip/1`. Unknown types become `Unknown`; unknown fields are ignored.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Msg {
    Hello {
        name: String,
        platform: Platform,
        protocol: u32,
        #[serde(default)]
        features: Vec<String>,
    },
    Clip(ClipMsg),
    Offer(OfferMsg),
    Request {
        id: String,
    },
    Peers {
        devices: Vec<PeerInfo>,
    },
    Unpair {
        id: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClipMsg {
    pub id: String,
    pub time_ms: u64,
    #[serde(default)]
    pub formats: Vec<FormatPart>,
    #[serde(default)]
    pub files: Vec<FilePart>,
    /// Sent on connect: apply it only if it's newer than the newest item the receiver has.
    #[serde(default, skip_serializing_if = "is_false")]
    pub latest: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OfferMsg {
    pub id: String,
    pub time_ms: u64,
    pub size: u64,
    #[serde(default)]
    pub mimes: Vec<String>,
    #[serde(default)]
    pub file_names: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub latest: bool,
}

/// One format in a `clip` header.
#[derive(Debug, Serialize, Deserialize)]
pub struct FormatPart {
    pub mime: String,
    pub size: u64,
}

/// One file in a `clip` header.
#[derive(Debug, Serialize, Deserialize)]
pub struct FilePart {
    pub name: String,
    pub size: u64,
}

/// A paired device, as sent in `peers` and `paired`, and as saved in `peers.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub platform: Platform,
    /// "ip:port" strings; ones we can't parse are skipped.
    #[serde(default)]
    pub addrs: Vec<String>,
}

/// A message on `sharedclip/pair/1`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PairMsg {
    Pair {
        secret: String,
        name: String,
        platform: Platform,
    },
    Paired {
        name: String,
        platform: Platform,
        #[serde(default)]
        devices: Vec<PeerInfo>,
    },
    Rejected,
    PairCode {
        commit: String,
        name: String,
        platform: Platform,
    },
    Nonce {
        nonce: String,
        name: String,
        platform: Platform,
    },
    Reveal {
        nonce: String,
    },
    Confirm {
        accept: bool,
    },
    #[serde(other)]
    Unknown,
}

fn is_false(b: &bool) -> bool {
    !b
}

pub fn net(e: impl std::fmt::Display) -> Error {
    Error::Network(e.to_string())
}

/// Writes `[u32 big-endian length][JSON]`.
pub async fn write_frame(send: &mut SendStream, msg: &impl Serialize) -> Result<(), Error> {
    let json = serde_json::to_vec(msg).map_err(net)?;
    send.write_all(&(json.len() as u32).to_be_bytes()).await.map_err(net)?;
    send.write_all(&json).await.map_err(net)
}

/// Reads `[u32 big-endian length][JSON]`, refusing anything longer than `max`.
pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream, max: usize) -> Result<T, Error> {
    let mut len = [0; 4];
    recv.read_exact(&mut len).await.map_err(net)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err(net("message too big"));
    }
    let mut buf = vec![0; len];
    recv.read_exact(&mut buf).await.map_err(net)?;
    serde_json::from_slice(&buf).map_err(net)
}

/// Sends a message without a body on its own stream.
pub async fn send_msg(conn: &Connection, msg: &Msg) -> Result<(), Error> {
    let mut send = conn.open_uni().await.map_err(net)?;
    write_frame(&mut send, msg).await?;
    send.finish().map_err(net)
}

/// Starts one format or file in the item id: its name, a 0 byte and its length. Its data follows.
pub fn hash_part(h: &mut blake3::Hasher, name: &str, len: u64) {
    h.update(name.as_bytes());
    h.update(&[0]);
    h.update(&len.to_le_bytes());
}

/// Longest device name we keep, in characters.
pub const MAX_NAME: usize = 64;
/// Longest file name we hand to the app, in characters.
pub const MAX_FILE_NAME: usize = 200;

/// A name another device sent, without control characters (they could drive a terminal) or text-direction
/// overrides, and at most `max` characters.
pub fn clean(name: &str, max: usize) -> String {
    let bad = |c: char| c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    name.chars().filter(|&c| !bad(c)).take(max).collect()
}

/// An item id is 64 lowercase hex characters; we check it before using it in a path.
pub fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Total size of an item's formats and files.
pub fn size(clip: &Clip) -> u64 {
    clip.formats.iter().map(|f| f.data.len() as u64).sum::<u64>() + clip.files.iter().map(|f| f.size).sum::<u64>()
}

/// Text only: goes right away even to a metered device.
pub fn is_text(clip: &Clip) -> bool {
    clip.files.is_empty() && clip.formats.iter().all(|f| f.mime.starts_with("text/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_types_and_fields_are_ignored() {
        let msg: Msg = serde_json::from_str(r#"{"type":"future-thing","x":1}"#).unwrap();
        assert!(matches!(msg, Msg::Unknown));
        let msg: Msg = serde_json::from_str(r#"{"type":"request","id":"ab","extra":[1,2]}"#).unwrap();
        assert!(matches!(msg, Msg::Request { id } if id == "ab"));
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean("Anil's\x1b[31m Mac\n", MAX_NAME), "Anil's[31m Mac");
        assert_eq!(clean("a\u{202e}b", MAX_NAME), "ab");
        assert_eq!(clean(&"x".repeat(100), MAX_NAME).len(), 64);
    }

    #[test]
    fn clip_header_shape() {
        let msg = Msg::Clip(ClipMsg {
            id: "x".into(),
            time_ms: 5,
            formats: vec![FormatPart {
                mime: "text/plain".into(),
                size: 2,
            }],
            files: vec![],
            latest: false,
        });
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"clip","id":"x","time_ms":5,"formats":[{"mime":"text/plain","size":2}],"files":[]}"#
        );
        let pair = PairMsg::PairCode {
            commit: "c".into(),
            name: "n".into(),
            platform: Platform::Macos,
        };
        assert_eq!(
            serde_json::to_string(&pair).unwrap(),
            r#"{"type":"pair-code","commit":"c","name":"n","platform":"macos"}"#
        );
    }
}
