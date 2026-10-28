//! Bluetooth LE, for phones that can't reach this computer over the network: this computer advertises SERVICE, its
//! GATT service tells the L2CAP channel (PSM) to connect to, and each channel a phone opens goes to the node.
//!
//! It never turns Bluetooth on: while there's no adapter or it's off, it waits. Any problem here is only logged, so
//! syncing over the network goes on.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bluer::adv::{Advertisement, AdvertisementHandle};
use bluer::gatt::local::{Application, ApplicationHandle, Characteristic, CharacteristicRead, Service};
use bluer::l2cap::{SocketAddr, Stream, StreamListener};
use bluer::{Adapter, AdapterEvent, AdapterProperty, ErrorKind, Session, SessionEvent, Uuid};
use futures_util::StreamExt;
use sharedclip::Node;

const SERVICE: Uuid = Uuid::from_u128(0x556874fd_9b25_4150_bb6d_47e1ba6fe50c);
/// Read-only: the PSM, 2 bytes little-endian.
const PSM: Uuid = Uuid::from_u128(0x940c7880_bb22_4995_8a3f_f76155b49830);
/// An advertisement has 31 bytes: flags take 3, the 128-bit SERVICE 18 and the name's header 2.
const NAME_ROOM: usize = 31 - 3 - 18 - 2;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Runs until dropped. Dropping it closes the listener and removes the advertisement and the GATT service.
pub async fn run(node: Arc<Node>) {
    let mut log = Log::default();
    loop {
        let Err(err) = watch(&node, &mut log).await;
        log.say(format!("Bluetooth: {err}"));
        // ponytail: bluetoothd missing or failing: try again now and then.
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// Serves on the default adapter whenever it's on.
async fn watch(node: &Arc<Node>, log: &mut Log) -> Res<Infallible> {
    let session = Session::new().await?;
    let mut sessions = Box::pin(session.events().await?);
    loop {
        let adapter = match session.default_adapter().await {
            Err(err) if err.kind == ErrorKind::NotFound => {
                log.say("Bluetooth: no adapter; waiting for one".into());
                loop {
                    match sessions.next().await {
                        Some(SessionEvent::AdapterAdded(_)) => break,
                        Some(_) => {}
                        None => return Err("lost bluetoothd".into()),
                    }
                }
                continue;
            }
            adapter => adapter?,
        };
        // Watch before looking, so a change in between isn't missed. The stream ends when the adapter goes.
        let mut events = adapter.events().await?;
        if adapter.is_powered().await? {
            serve(node, log, &adapter, &mut events).await?;
        } else {
            log.say("Bluetooth is off; waiting for it to be turned on".into());
            while let Some(event) = events.next().await {
                if matches!(event, AdapterEvent::PropertyChanged(AdapterProperty::Powered(true))) {
                    break;
                }
            }
        }
    }
}

/// Serves until the adapter is turned off or removed.
async fn serve(
    node: &Arc<Node>,
    log: &mut Log,
    adapter: &Adapter,
    events: &mut (impl futures_util::Stream<Item = AdapterEvent> + Unpin),
) -> Res<()> {
    let (listener, psm, _gatt, _adv) = publish(adapter).await?;
    log.say(format!("Bluetooth: ready on {} (PSM {psm:#06x})", adapter.name()));
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, from)) => on_stream(node, stream, from),
                Err(err) => {
                    log.say(format!("Bluetooth: {err}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            event = events.next() => match event {
                None | Some(AdapterEvent::PropertyChanged(AdapterProperty::Powered(false))) => {
                    log.say("Bluetooth is off; waiting for it to be turned on".into());
                    return Ok(());
                }
                Some(_) => {}
            },
        }
    }
}

/// Listens, then publishes the PSM in the GATT service and advertises. Dropping what it returns undoes it all.
async fn publish(adapter: &Adapter) -> Res<(StreamListener, u16, ApplicationHandle, AdvertisementHandle)> {
    let (listener, psm) = listen(adapter).await?;
    let read = CharacteristicRead {
        read: true,
        fun: Box::new(move |req| {
            let value = psm.to_le_bytes();
            Box::pin(async move { Ok(value.get(usize::from(req.offset)..).unwrap_or_default().to_vec()) })
        }),
        ..Default::default()
    };
    let app = Application {
        services: vec![Service {
            uuid: SERVICE,
            primary: true,
            characteristics: vec![Characteristic {
                uuid: PSM,
                read: Some(read),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let gatt = adapter.serve_gatt_application(app).await?;
    let name = adapter.alias().await.unwrap_or_default();
    let adv = Advertisement {
        service_uuids: [SERVICE].into(),
        discoverable: Some(true),
        // A name that doesn't fit is left out, not cut.
        local_name: (!name.is_empty() && name.len() <= NAME_ROOM).then_some(name),
        ..Default::default()
    };
    let adv = adapter.advertise(adv).await?;
    Ok((listener, psm, gatt, adv))
}

/// An L2CAP LE listener on the adapter, on the first free PSM from 0x85.
async fn listen(adapter: &Adapter) -> Res<(StreamListener, u16)> {
    let (addr, addr_type) = (adapter.address().await?, adapter.address_type().await?);
    for psm in 0x85..=bluer::l2cap::PSM_LE_MAX {
        match StreamListener::bind(SocketAddr::new(addr, addr_type, psm)).await {
            Ok(listener) => return Ok((listener, psm)),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(err) => return Err(err.into()),
        }
    }
    Err("no free L2CAP PSM".into())
}

/// A phone opened a channel: the node runs the link on it.
fn on_stream(node: &Arc<Node>, stream: Stream, from: SocketAddr) {
    eprintln!("Bluetooth: channel from {}", from.addr);
    // PLACEHOLDER until the core has it: replace with `node.bluetooth_link(stream, false);`
    drop((node, stream));
}

/// Prints each message once, until a different one comes.
#[derive(Default)]
struct Log(String);

impl Log {
    fn say(&mut self, msg: String) {
        if msg != self.0 {
            eprintln!("{msg}");
            self.0 = msg;
        }
    }
}
