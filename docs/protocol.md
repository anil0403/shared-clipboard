# Wire protocol, version 1

How Shared Clipboard devices talk to each other. The version here is `PROTOCOL_VERSION` in `core/src/lib.rs`, not the
release version (see [VERSIONING.md](../VERSIONING.md#the-protocol-version-is-different)).

## Connections
- Devices connect with [iroh](https://iroh.computer) 1.x (QUIC). A device's **id** is its iroh endpoint id: an
  Ed25519 public key, as text. TLS inside QUIC proves both ids, so a connection's peer id can be trusted.
- On the local network, relays and n0's DNS discovery are off. Addresses come from the apps' own discovery (below) or
  from a pairing link.
- Two ALPNs:
  - `sharedclip/1`: paired devices. A connection from an id that isn't paired is closed right away.
  - `sharedclip/pair/1`: pairing. Anyone may connect, but nothing happens without the link's secret or the code check.
- The UDP port is 47820 if it's free (so firewalls can open one port), else any free port.
- **One connection per pair of devices.** Either side may connect. If two connections between the same devices
  exist and the second came within 3 seconds of the first (both dialed at once), both sides keep the one opened by
  the device whose id (as text) sorts lower, and close the other. Otherwise the second is a reconnect, and the newer
  one is kept: the old one may be dead without either side knowing yet.

## Finding devices
Every app publishes and browses the DNS-SD service `_sharedclip._udp` on the local network, with the node's port and
these TXT entries:

| Key | Value |
|---|---|
| `id` | the device id |
| `name` | the device name |
| `platform` | `linux`, `macos`, `android` or `ios` |

The app passes each device it finds to the core (`Node::found`).

## Messages (`sharedclip/1`)
Each message is its own **unidirectional QUIC stream**, so a large item never holds up a small one:

```
[u32, big-endian: header length][header: JSON object][body: raw bytes, to the end of the stream]
```

The header has a `type`. A device ignores a message whose `type` it doesn't know, and fields it doesn't know, so newer
apps can add things without breaking older ones.

| `type` | Fields | Body | Meaning |
|---|---|---|---|
| `hello` | `name`, `platform`, `protocol`, `features` (list) | none | Sent first by both sides on every connection. May be sent again at any time; it replaces the earlier one |
| `clip` | `id`, `time_ms`, `formats`: [{`mime`, `size`}], `files`: [{`name`, `size`}], `latest` (optional) | the formats' data in order, then the files' data in order | A clipboard item |
| `offer` | `id`, `time_ms`, `size`, `mimes`, `file_names`, `latest` (optional) | none | An item that wasn't sent: too big, or the receiver is on mobile data |
| `request` | `id` | none | Send me this item; answered with a `clip`, or `missing` |
| `missing` | `id` | none | Answers a `request` for an item the device doesn't have (any more), or while it's paused, so the asking device stops waiting |
| `peers` | `devices`: [{`id`, `name`, `platform`, `addrs`}] | none | These devices are paired with me; trust them too |
| `unpair` | `id` | none | Forget device `id`. If `id` is the sender, it forgot you. One naming the receiver is ignored |

The only feature so far is `metered`: the device is on mobile data. A device sends a new `hello` when that changes.

### Items
- **Item id:** BLAKE3, in hex, over each format (`mime` bytes, a 0 byte, its length as u64 little-endian, its data) and
  then each file (`name` bytes, a 0 byte, its length as u64 little-endian, its data). The same content has the same
  id everywhere. A receiver checks the id against the data and drops the item if they don't match.
- **Sending:** items up to 25 MiB (`SIZE_CAP`) go as a `clip` right away. Bigger items go as an `offer`. When the
  receiver is on mobile data (it said so with the `metered` feature in its latest `hello`), only items that are all
  text formats (`text/*`, no files) go right away. A `clip` that answers a `request` may be bigger than 25 MiB.
- **Newest wins:** a receiver puts each live item (no `latest`) on the clipboard, in the order they arrive.
- **Catch-up:** when a device gets the first `hello` on a connection, it sends its newest item, as a `clip` or an
  `offer` by the rules above, with `"latest": true`. The receiver uses it only if its `time_ms` is newer than the
  newest item it has, so a device that was away gets what was copied meanwhile without undoing newer copies.
- **Files:** a receiver saves files under its own folder and makes each name safe for that: `/`, `\` and control
  characters become `_`, a name is cut to 200 bytes, and an empty name, `.` or `..` becomes `file`. The item keeps
  the original `name`, so its id doesn't change.
- **Names:** before showing a device or file name from another device, a receiver drops control characters and
  text-direction overrides (they could drive a terminal or disguise the name) and cuts device names to 64 characters.
- **Limits:** a header is at most 64 KB. A `clip` body bigger than 25 MiB plus 1 MiB is refused, unless it answers a
  `request` (then the offer's size plus 1 MiB). A pairing message is at most 64 KB.
- **No echo:** when the app reports a clipboard change whose id is one of the last 8 items the core gave it, the core
  doesn't send it again.
- **Not sent:** password-manager items. The apps leave them out before they reach the core.

## Bluetooth LE
For paired devices that are near each other but can't reach each other over the network (e.g. a guest Wi-Fi, or a
phone that isn't on the desktop's network). The desktop is the peripheral and the phone the central. There's no
Bluetooth pairing or bonding: the device keys encrypt and authenticate the link.

### Finding a desktop
- The desktop advertises (connectable) the 128-bit service UUID `556874fd-9b25-4150-bb6d-47e1ba6fe50c`. Its GATT
  service with that UUID has one read-only characteristic, `940c7880-bb22-4995-8a3f-f76155b49830`: the L2CAP PSM to
  connect to, as 2 bytes little-endian. (Apple publishes channel numbers the same way.)
- The desktop listens for L2CAP LE credit-based connections on that PSM, without Bluetooth security. The PSM is
  dynamic: the first free one from `0x0085` up to `0xFF`.
- The phone scans for the service UUID (low power) only while some paired device isn't connected over the network
  (`Node::bluetooth_wanted`). On a result it connects over GATT, reads the PSM, closes GATT and opens the L2CAP
  channel. It keeps one link per desktop; when that closes, it scans again, waiting 5 s, doubling up to 60 s.

### The link
The L2CAP channel is a reliable byte stream. On it:

1. **Handshake:** `Noise_XX_25519_ChaChaPoly_BLAKE2s`, with empty payloads. The phone is the initiator. Each
   device's static key is the X25519 form of its Ed25519 key: the secret is the first 32 bytes of SHA-512 of the
   32-byte seed, clamped; the public key is the Ed25519 public key converted from Edwards to Montgomery form. Every
   Noise message, in the handshake and after it, is `[u16, big-endian: length][message]`, at most 65535 bytes.
2. **Trust:** each side looks for the paired device whose id, in Montgomery form, is the other side's static key. If
   there's none (not paired, or removed), it closes the link. A newer link to the same device replaces an older one.
3. **Messages:** the same as on `sharedclip/1`, one after another on one stream:
   `[u32, big-endian: header length][header: JSON object][body]`. Only `clip` has a body (its formats' data, in
   order), and its header gives the body's length. The stream is cut into Noise messages of at most 65519 bytes
   before encryption (65535 minus the 16-byte tag); the nonces count up from 0 in each direction.
4. **What goes over it:** only items for a device with no network connection. Items up to 1 MiB without files go as
   a `clip`; anything else goes as an `offer`, fetched later over the network. A `request` over Bluetooth gets
   `missing` unless the item is up to 1 MiB without files. A `clip` bigger than that, or with files, closes the
   link.
5. `hello`, catch-up, no echo and the history work as on the network. Catch-up goes over Bluetooth only if there's
   no network connection. When the network connection comes back, new items go over it, and the Bluetooth link
   stays open, unused.
6. **Timeouts:** the handshake 15 s; a message 30 s plus 60 s per MiB. A side that sent nothing for 60 s sends its
   `hello` again; a link that received nothing for 150 s is closed.

## Pairing (`sharedclip/pair/1`)
One bidirectional stream. Each message is `[u32, big-endian: length][JSON object]`.

### With a link (QR code)
The link is shown as a QR code by the device being joined (A):

```
sharedclip://pair?id=<A's id>&addr=<ip:port>[,<ip:port>…]&secret=<base32, 16 random bytes>
```

`addr` lists A's network interface addresses (not loopback or link-local, unless there's nothing else) with its port.
The secret is base32 (RFC 4648, upper case, no padding). The link is valid for 10 minutes and for one pairing, and a
new link replaces the previous one. The joining device (B) connects to A's id at those addresses, so TLS proves it
reached A. A device handles one pairing at a time; another one meanwhile gets `rejected`.

1. B → A: `{"type":"pair","secret":…,"name":…,"platform":…}`
2. A checks the secret. Then A → B: `{"type":"paired","name":…,"platform":…,"devices":[…]}` with A's other paired
   devices, or `{"type":"rejected"}`. After 5 wrong secrets the link stops working.
3. Both save each other. B also trusts the listed devices. A sends `peers` with B to its other paired devices.

### With a code (two devices without a camera)
B found A through discovery and connects. A answers only while it shows a pairing link (its pair screen is open);
otherwise it replies `rejected` at once, without asking its user, so a stranger can't pop up prompts.

1. B → A: `{"type":"pair-code","commit":<hex BLAKE3 of nonce_b>,"name":…,"platform":…}`
2. A → B: `{"type":"nonce","nonce":<nonce_a, hex, 32 random bytes>,"name":…,"platform":…}`
3. B → A: `{"type":"reveal","nonce":<nonce_b, hex, 32 random bytes>}`. A checks it against the commit.
4. Both compute `code` = the first 4 bytes of BLAKE3(`"sharedclip pair"` ‖ A's key ‖ B's key ‖ nonce_a ‖ nonce_b), as
   a u32 big-endian, mod 1,000,000, shown as 6 digits. The keys are the 32 raw public-key bytes, the nonces the 32 raw
   bytes (not their text). Both show it.
5. Each side sends `{"type":"confirm","accept":true|false}` when its user answers. They pair only if both accept
   within 2 minutes. Then A sends `paired` (with its other devices) or `rejected`, as in step 2 of the link flow, and
   step 3 follows.

B commits to its nonce before seeing A's, so someone in the middle can't pick nonces to make the codes match.

## Trust
- A device trusts the devices it paired with, and the devices a paired device introduced with `peers`.
- `unpair` from a paired device removes the device it names.
- When the user unpairs a device, the core sends `unpair` with that device's id to its other connected devices, and
  `unpair` with its own id to the removed one, then closes the connection. Devices that are offline then aren't told.
- A device remembers the devices it removed (by its user or by `unpair`) and ignores them in `peers` and `paired`, so
  a device that was offline can't bring them back. Only pairing with that device directly again trusts it again.
