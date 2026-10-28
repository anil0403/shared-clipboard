//! Pairing (`sharedclip/pair/1`): with a link shown as a QR code, or by comparing a 6-digit code.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use data_encoding::{BASE32_NOPAD, HEXLOWER, HEXLOWER_PERMISSIVE};
use iroh::{
    EndpointAddr, PublicKey, TransportAddr,
    endpoint::{Connection, RecvStream, SendStream, VarInt},
};
use tokio::{sync::oneshot, time::timeout};

use crate::{
    Device, Error,
    node::{Inner, parse_addrs},
    protocol::{MAX_NAME, MAX_PAIR_MSG, PAIR_ALPN, PairMsg, PeerInfo, clean, net, read_frame, write_frame},
};

const LINK_TTL: Duration = Duration::from_secs(10 * 60);
/// Wrong secrets before the link stops working.
const MAX_WRONG: u32 = 5;
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// How long we wait for each step of the other side.
const STEP_TIMEOUT: Duration = Duration::from_secs(15);

/// The pairing link that works right now. A new one replaces the old one.
pub struct Link {
    secret: String,
    expires: Instant,
    wrong: u32,
}

pub fn link(inner: &Inner) -> String {
    let secret = BASE32_NOPAD.encode(&rand::random::<[u8; 16]>());
    inner.lock().link = Some(Link {
        secret: secret.clone(),
        expires: Instant::now() + LINK_TTL,
        wrong: 0,
    });
    let addrs: Vec<String> = local_addrs(inner).iter().map(|a| a.to_string()).collect();
    format!(
        "sharedclip://pair?id={}&addr={}&secret={secret}",
        inner.id,
        addrs.join(",")
    )
}

/// Our addresses on the local network: every interface IP but loopback, with our port. Loopback only when there's
/// nothing else, so a machine without a network can still pair with itself in tests.
fn local_addrs(inner: &Inner) -> Vec<SocketAddr> {
    let bound = inner.ep.bound_sockets();
    let port = |ip: &IpAddr| bound.iter().find(|a| a.is_ipv4() == ip.is_ipv4()).map(|a| a.port());
    let ips = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_link_local())
        .map(|i| i.ip());
    let (lo, lan): (Vec<IpAddr>, Vec<IpAddr>) = ips.partition(|ip| ip.is_loopback());
    let ips = if lan.is_empty() { lo } else { lan };
    ips.into_iter()
        .filter_map(|ip| Some(SocketAddr::new(ip, port(&ip)?)))
        .collect()
}

fn parse_link(link: &str) -> Option<(PublicKey, Vec<SocketAddr>, String)> {
    let query = link.trim().strip_prefix("sharedclip://pair?")?;
    let (mut id, mut addrs, mut secret) = (None, Vec::new(), None);
    for pair in query.split('&') {
        match pair.split_once('=')? {
            ("id", v) => id = v.parse().ok(),
            ("addr", v) => addrs = v.split(',').filter_map(|a| a.parse().ok()).collect(),
            ("secret", v) if !v.is_empty() => secret = Some(v.to_string()),
            _ => {}
        }
    }
    Some((id?, addrs, secret?))
}

/// Joins the device that showed `link`.
pub async fn pair_link(inner: &Arc<Inner>, link: &str) -> Result<Device, Error> {
    let (id, addrs, secret) = parse_link(link).ok_or(Error::BadLink)?;
    let _busy = Busy::start(inner)?;
    let (conn, mut send, mut recv) = open(inner, id, &addrs).await?;
    write_frame(
        &mut send,
        &PairMsg::Pair {
            secret,
            name: inner.name.clone(),
            platform: inner.platform,
        },
    )
    .await?;
    let reply = step(&mut recv).await;
    conn.close(VarInt::from_u32(0), b"done");
    joined(inner, id, &addrs, reply?)
}

/// Pairs with a device found by discovery, comparing a code. We are B in `docs/protocol.md`.
pub async fn pair_device(inner: &Arc<Inner>, id: &str) -> Result<Device, Error> {
    let key: PublicKey = id.parse().map_err(|_| Error::UnknownDevice)?;
    let addrs = {
        let s = inner.lock();
        let id = key.to_string();
        let addrs = s.found.get(&id).or(s.peers.get(&id).map(|p| &p.addrs));
        addrs.map(|a| parse_addrs(a)).unwrap_or_default()
    };
    if addrs.is_empty() {
        return Err(Error::UnknownDevice);
    }
    let _busy = Busy::start(inner)?;
    let (conn, mut send, mut recv) = open(inner, key, &addrs).await?;
    let nonce_b: [u8; 32] = rand::random();
    let commit = blake3::hash(&nonce_b).to_hex().to_string();
    write_frame(
        &mut send,
        &PairMsg::PairCode {
            commit,
            name: inner.name.clone(),
            platform: inner.platform,
        },
    )
    .await?;
    let PairMsg::Nonce { nonce, name, platform } = step(&mut recv).await? else {
        return Err(Error::PairingRejected);
    };
    let nonce_a = decode_nonce(&nonce)?;
    let name = clean(&name, MAX_NAME);
    write_frame(
        &mut send,
        &PairMsg::Reveal {
            nonce: HEXLOWER.encode(&nonce_b),
        },
    )
    .await?;
    let code = code(&key, &inner.ep.id(), &nonce_a, &nonce_b);
    let device = Device {
        id: key.to_string(),
        name,
        platform,
        online: true,
    };
    if !confirm(inner, device, code, &mut send, &mut recv).await? {
        return Err(Error::PairingRejected);
    }
    let reply = step(&mut recv).await;
    conn.close(VarInt::from_u32(0), b"done");
    joined(inner, key, &addrs, reply?)
}

pub fn confirm_pairing(inner: &Inner, id: &str, accept: bool) {
    if let Some(tx) = inner.lock().confirms.remove(id) {
        let _ = tx.send(accept);
    }
}

/// The joining side's last step: trust the device we joined and the devices it listed, and connect to them.
fn joined(inner: &Arc<Inner>, id: PublicKey, addrs: &[SocketAddr], reply: PairMsg) -> Result<Device, Error> {
    let PairMsg::Paired {
        name,
        platform,
        devices,
    } = reply
    else {
        return Err(Error::PairingRejected);
    };
    let name = clean(&name, MAX_NAME);
    let addrs = addrs.iter().map(|a| a.to_string()).collect();
    let peer = PeerInfo {
        id: id.to_string(),
        name: name.clone(),
        platform,
        addrs,
    };
    inner.add_paired(peer);
    let introduced = inner.add_peers(devices);
    inner.emit_devices();
    inner.connect(&id.to_string());
    for id in introduced {
        inner.connect(&id);
    }
    Ok(Device {
        id: id.to_string(),
        name,
        platform,
        online: false,
    })
}

/// Answers a device that connected on `sharedclip/pair/1`. We are A in `docs/protocol.md`.
pub async fn respond(inner: Arc<Inner>, conn: Connection, addr: Option<SocketAddr>) {
    let Ok(Ok((mut send, mut recv))) = timeout(STEP_TIMEOUT, conn.accept_bi()).await else {
        return;
    };
    // Read the first message before taking the pairing slot, so a device that says nothing doesn't hold it.
    let first = step(&mut recv).await;
    let peer = match (first, Busy::start(&inner)) {
        (Ok(first), Ok(_busy)) => check(&inner, &conn, addr, first, &mut send, &mut recv)
            .await
            .ok()
            .flatten(),
        _ => None,
    };
    let reply = match &peer {
        Some(peer) => {
            let devices = inner
                .lock()
                .peers
                .values()
                .filter(|p| p.id != peer.id)
                .cloned()
                .collect();
            inner.add_paired(peer.clone());
            PairMsg::Paired {
                name: inner.name.clone(),
                platform: inner.platform,
                devices,
            }
        }
        None => PairMsg::Rejected,
    };
    // Wait for the other side to read our answer and close.
    if write_frame(&mut send, &reply).await.is_ok() && send.finish().is_ok() {
        let _ = timeout(STEP_TIMEOUT, conn.closed()).await;
    }
    if let Some(peer) = peer {
        inner.emit_devices();
        inner.introduce(&peer);
    }
}

/// The responder's side up to the decision. Returns the device if it may pair.
async fn check(
    inner: &Inner,
    conn: &Connection,
    addr: Option<SocketAddr>,
    first: PairMsg,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<Option<PeerInfo>, Error> {
    let id = conn.remote_id();
    let peer = |name: String, platform| PeerInfo {
        id: id.to_string(),
        name: clean(&name, MAX_NAME),
        platform,
        addrs: addr.iter().map(|a| a.to_string()).collect(),
    };
    match first {
        PairMsg::Pair { secret, name, platform } => Ok(take_link(inner, &secret).then(|| peer(name, platform))),
        // Only while this device shows a pairing link (its pair screen is open), so a stranger on the network can't
        // pop up pairing prompts, or hold the one pairing slot, whenever they like.
        PairMsg::PairCode { commit, name, platform } if link_open(inner) => {
            let nonce_a: [u8; 32] = rand::random();
            let nonce = HEXLOWER.encode(&nonce_a);
            write_frame(
                send,
                &PairMsg::Nonce {
                    nonce,
                    name: inner.name.clone(),
                    platform: inner.platform,
                },
            )
            .await?;
            let PairMsg::Reveal { nonce } = step(recv).await? else {
                return Ok(None);
            };
            let nonce_b = decode_nonce(&nonce)?;
            if !blake3::hash(&nonce_b).to_hex().eq_ignore_ascii_case(&commit) {
                return Ok(None);
            }
            let code = code(&inner.ep.id(), &id, &nonce_a, &nonce_b);
            let device = Device {
                id: id.to_string(),
                name: clean(&name, MAX_NAME),
                platform,
                online: true,
            };
            Ok(confirm(inner, device, code, send, recv)
                .await?
                .then(|| peer(name, platform)))
        }
        _ => Ok(None),
    }
}

/// Checks a secret against the current link. The right one uses the link up; after 5 wrong ones it stops working.
fn take_link(inner: &Inner, secret: &str) -> bool {
    let mut s = inner.lock();
    let Some(link) = s.link.as_mut() else { return false };
    let live = link.expires > Instant::now();
    let ok = live && same(link.secret.as_bytes(), secret.as_bytes());
    link.wrong += u32::from(!ok);
    if ok || !live || link.wrong >= MAX_WRONG {
        s.link = None;
    }
    ok
}

fn link_open(inner: &Inner) -> bool {
    inner.lock().link.as_ref().is_some_and(|l| l.expires > Instant::now())
}

/// Compares in constant time, so the time to answer doesn't give the secret away.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Shows the code on both devices and waits for both users. True if both accept within 2 minutes.
async fn confirm(
    inner: &Inner,
    device: Device,
    code: String,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<bool, Error> {
    let id = device.id.clone();
    let (tx, rx) = oneshot::channel();
    inner.lock().confirms.insert(id.clone(), tx);
    inner.events.on_pairing_code(device, code);
    let answer = timeout(CONFIRM_TIMEOUT, answers(rx, send, recv))
        .await
        .unwrap_or(Ok(false));
    inner.lock().confirms.remove(&id);
    answer
}

/// Sends our user's answer when it comes, and reads the other side's. Stops at the first "no".
async fn answers(
    mut mine: oneshot::Receiver<bool>,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<bool, Error> {
    let mut theirs = std::pin::pin!(read_frame::<PairMsg>(recv, MAX_PAIR_MSG));
    let (mut me, mut them) = (None, None);
    loop {
        tokio::select! {
            accept = &mut mine, if me.is_none() => {
                let accept = accept.unwrap_or(false);
                write_frame(send, &PairMsg::Confirm { accept }).await?;
                me = Some(accept);
            }
            msg = &mut theirs, if them.is_none() => them = Some(matches!(msg?, PairMsg::Confirm { accept: true })),
        }
        match (me, them) {
            (Some(false), _) | (_, Some(false)) => return Ok(false),
            (Some(true), Some(true)) => return Ok(true),
            _ => {}
        }
    }
}

/// The 6-digit code. `a` is the device being joined, `b` the one joining.
fn code(a: &PublicKey, b: &PublicKey, nonce_a: &[u8], nonce_b: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"sharedclip pair");
    h.update(a.as_bytes());
    h.update(b.as_bytes());
    h.update(nonce_a);
    h.update(nonce_b);
    let x = h.finalize();
    let x = x.as_bytes();
    format!("{:06}", u32::from_be_bytes([x[0], x[1], x[2], x[3]]) % 1_000_000)
}

fn decode_nonce(nonce: &str) -> Result<Vec<u8>, Error> {
    HEXLOWER_PERMISSIVE
        .decode(nonce.as_bytes())
        .ok()
        .filter(|n| n.len() == 32)
        .ok_or(Error::PairingRejected)
}

async fn open(
    inner: &Inner,
    id: PublicKey,
    addrs: &[SocketAddr],
) -> Result<(Connection, SendStream, RecvStream), Error> {
    let addr = EndpointAddr::from_parts(id, addrs.iter().copied().map(TransportAddr::Ip));
    let conn = timeout(STEP_TIMEOUT, inner.ep.connect(addr, PAIR_ALPN))
        .await
        .map_err(net)?
        .map_err(net)?;
    let (send, recv) = conn.open_bi().await.map_err(net)?;
    Ok((conn, send, recv))
}

async fn step(recv: &mut RecvStream) -> Result<PairMsg, Error> {
    timeout(STEP_TIMEOUT, read_frame(recv, MAX_PAIR_MSG))
        .await
        .map_err(net)?
}

/// Only one pairing runs at a time; this marks it running until dropped.
struct Busy<'a>(&'a Inner);

impl<'a> Busy<'a> {
    fn start(inner: &'a Inner) -> Result<Self, Error> {
        let mut s = inner.lock();
        if s.pairing {
            return Err(Error::PairingRejected);
        }
        s.pairing = true;
        Ok(Busy(inner))
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.lock().pairing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_parse() {
        let id = iroh::SecretKey::generate().public();
        let link = format!("sharedclip://pair?id={id}&addr=192.168.1.5:47820,[fe80::1]:47820&secret=ABC");
        let (got, addrs, secret) = parse_link(&link).unwrap();
        assert_eq!((got, addrs.len(), secret.as_str()), (id, 2, "ABC"));
        assert!(parse_link("https://example.com").is_none());
        assert!(parse_link(&format!("sharedclip://pair?id={id}&addr=1.2.3.4:5")).is_none());
    }

    #[test]
    fn codes_are_six_digits() {
        let (a, b) = (
            iroh::SecretKey::generate().public(),
            iroh::SecretKey::generate().public(),
        );
        let code = code(&a, &b, &[1; 32], &[2; 32]);
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|c| c.is_ascii_digit()));
    }
}
