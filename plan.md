# Shared Clipboard: plan

Copy on one device, paste on another, fast. Works with noobsbot-desktop (Linux, Wayland), macOS, Android and iOS.
Shares text, links, rich text, images and files.

Every device runs a small background app that stays connected to your other devices, so a copy reaches them right
away. Devices connect over Bluetooth, over the same network (with or without internet), or later over the internet
through accounts.

Status: plan, no code yet. Second version, 2026-10-28, based on research into each platform, the connection library
and Bluetooth. Anything marked "not confirmed" gets tested in Phase 0 before we rely on it.

## Background
- noobsbot-desktop keeps the clipboard in the compositor, up to 10 MB (`src/clipboard.rs`). It already has a tray
  host and a trusted-client socket, but it doesn't yet let an outside program watch the clipboard. Smithay 0.7 already
  includes `ext_data_control`, so adding that is a small change.
- KDE Connect, Paste and LocalSend all hit the same iOS limits we do (see [iOS](#ios)). Building our own is worth it
  for always-on syncing on the other platforms, images and files, connecting over Bluetooth, and accounts.

## 1. What it does
- You copy on one device and paste on another. On the desktops and Android, the item arrives on its own; iOS needs
  one tap (see [What each platform allows](#4-what-each-platform-allows)).
- It handles text, links, rich text (HTML), images and files.
- You pair each device once. Only paired devices can talk to each other.
- There's a shared history of the last 50 items; tap one to copy it again.
- Anything copied from a password manager is never sent.
- You can pause sharing, turn sending or receiving on or off for each device, or send an item to one device only.
- You can see which devices are online and how they're connected (same network, Bluetooth or internet).

### Speed targets
| Path | Text arrives within |
|---|---|
| Same network, both devices' apps running | 0.3 s (plus up to 0.5 s when sent from a Mac, which checks its clipboard every 0.5 s) |
| Bluetooth, already connected | 0.3 s (connecting first adds 1–5 s, not confirmed) |
| Internet, direct connection (accounts phase) | 1 s |
| Internet, through our relay (accounts phase) | 2 s |

## 2. Connections

### Three ways to connect
| Connection | When it's used | Good for |
|---|---|---|
| Same network | Devices on the same Wi-Fi, Ethernet or phone hotspot, with or without internet | Everything, including large files |
| Bluetooth LE | Nearby, but not on the same network, or the network blocks devices from reaching each other | Text and items up to about 1 MB; finding and waking nearby devices, including a sleeping iPhone app |
| Internet (accounts phase) | On different networks, both online through Wi-Fi or mobile data | Everything, subject to the mobile data rules below |

### Which connection is used in each situation
| Situation | Connection |
|---|---|
| Same Wi-Fi, with internet | Direct on the network |
| Same Wi-Fi, no internet | Direct on the network; works the same |
| Laptop joined to the phone's hotspot | Direct on the network |
| Wi-Fi that blocks devices from reaching each other (hotel or guest Wi-Fi) | Bluetooth, or internet if both are online |
| Different networks, both online | Internet: a direct connection through NAT first, our relay as fallback |
| Phone has no mobile data and isn't on the desktop's network | Bluetooth for text and items up to about 1 MB; bigger items wait for a network |
| No connection at all | Keep the latest item and send it when a connection comes back |

When there's a better connection available, a connection or transfer moves to it automatically, for example from
Bluetooth or the internet to the local network when you get home.

### Same network without internet
- Devices find each other with mDNS/Bonjour or Bluetooth and connect by local IP. Nothing looks up a server: iroh's
  discovery through n0's DNS servers and its relays both stay off on the local network.
- Pairing works offline: the QR code holds the device's key and its local addresses.
- Each device's key is pinned, so there's no certificate authority, expiry or revocation check that would need the
  internet or a correct clock.
- **Android:** when the Wi-Fi has no internet, Android sends app traffic over mobile data by default, so connections
  to local IPs fail. The Android app ties its sockets to the Wi-Fi network (`Network.bindSocket`).
- **iOS:** Wi-Fi Assist can move traffic to cellular. Local connections require the Wi-Fi interface.

### Mobile data
- The user may or may not have mobile data. Nothing depends on it.
- Mobile data counts as metered. By default, text and links sync over it, and images and files wait for Wi-Fi.
- A setting chooses what may use mobile data: everything, text only, or nothing.
- The app follows Android Data Saver and iOS Low Data Mode, and backs off on expensive or limited connections.
- A large file never goes over mobile data unless you tap Download.

### Bluetooth
The times below assume the devices are already connected. They come from developer reports on different hardware,
so measure on real devices before setting size limits.

| Link | Platforms | 1 KB / 2 MB / 50 MB | Use |
|---|---|---|---|
| BLE GATT | all 4 | under 0.3 s / 0.5–3 min / 15–70 min | presence, waking devices, control messages |
| BLE L2CAP channel | all 4 | under 0.1 s / 20–60 s / 8–25 min | text and items up to about 1 MB |
| Classic Bluetooth (RFCOMM) | not iOS (it needs Apple's MFi program) | — | not used |

- **Roles:** the Mac and Linux machines advertise our Bluetooth service, and the phones connect to them.
- **What Bluetooth carries:** presence, waking a device, control messages, and items up to about 1 MB. It never
  carries anything bigger. We don't use it only to keep the iOS app alive, which App Review guideline 2.5.4 forbids.
- **Handover:**
  - Over Bluetooth, the sender sends an offer: the item's hash and size, its local addresses and its key.
  - If the receiver can reach the sender over a network, it connects with iroh and fetches the item there.
  - Bluetooth carries the item itself only when there's no network and the item is small.
  - Apple Continuity and Google's Nearby Connections work the same way.
- **Encryption:** our own end-to-end encryption with the same device keys as iroh, not Bluetooth pairing. iOS apps
  can't start Bluetooth bonding, and pairing works differently on each platform. Choose between Noise (as Bitchat
  uses over Bluetooth) and TLS in Phase 0.
- **Messages:** the same messages as on the network, as length-prefixed frames over the L2CAP channel. GATT is the
  fallback.
- **Not now:** iroh-ble-transport would let iroh run over Bluetooth directly. It's experimental, AGPL-licensed and
  runs at about 100 kbit/s. Revisit it later.
- **Later, fast offline links:**
  - AWDL between iPhone and Mac, through Network.framework's peer-to-peer support.
  - Wi-Fi Aware between iPhone (iOS 26+, iPhone 12 or later) and Android. Whether it works between Apple and Android
    isn't confirmed.
  - Wi-Fi Direct is optional, and only works between Android and Linux.

## 3. Architecture

```
┌──────────────────── each device: background app ────────────────────┐
│ native layer: watch/write clipboard, find devices (Bonjour/NSD/mDNS,│  Rust (Linux) · Swift (Apple) · Kotlin (Android)
│ Bluetooth), stay alive in the background, UI                        │
├─────────────────────────────────────────────────────────────────────┤
│ shared core (Rust): identity, pairing, protocol, transfer, history, │  one crate, bound to Swift and Kotlin with UniFFI
│ dedupe, metered rules; iroh for connections                         │
└─────────────────────────────────────────────────────────────────────┘
      ⇅ iroh (QUIC): direct on the network now, plus internet and relay later
      ⇅ Bluetooth LE: L2CAP channel, same messages, same keys
```

### Connections: iroh 1.x
- **What iroh does:** you connect to a device by its public key. Connections are encrypted QUIC: text gets through
  while a large file is transferring, one connection carries many streams, and a connection survives switching
  between Wi-Fi and cellular. iroh also handles direct connections through NAT, with a relay as fallback.
- **Why iroh over plain TLS or QUIC:** with plain TLS or QUIC we'd build NAT traversal and a relay ourselves for the
  accounts phase. With iroh the same code that connects on the local network works over the internet later; we only
  turn relays on.
- **Status:** iroh is at version 1.4.1, and its wire format stays compatible across all of 1.x. It goes into our Rust
  core as a normal Rust library.
- **Local network now:** relays and n0's DNS discovery are off. We give iroh the addresses our own discovery finds.
- **Accounts later:** turn relays on and run our own `iroh-relay` in two regions. n0's public relays are rate-limited
  and not meant for production.
- **Risk:** one small company, n0, controls iroh's API, its QUIC stack and the default relays. Running our own relay
  limits that.
- **Firewalls:** QUIC uses UDP. The app uses a fixed UDP port so the Linux install docs can open it (ufw or firewalld).

### Finding devices
- Every platform publishes and browses one DNS-SD service type, with the device's key in the TXT record. Apple uses
  Bonjour (`NWBrowser`); raw mDNS on iOS needs a multicast entitlement Apple must approve. Android uses `NsdManager`,
  and Linux uses the `mdns-sd` crate. Using the same service type everywhere lets any device find any other.
- iroh's built-in mDNS uses its own format, which Apple's Bonjour wouldn't see, so we don't use it.
- Mac and Linux machines also advertise a fixed Bluetooth service, so phones find them without a network.
- **Add by IP** is a fallback when mDNS is blocked.

### Each app
| Platform | What runs | How it stays in the background |
|---|---|---|
| noobsbot-desktop | background service (systemd user service), tray icon, small iced window for pairing and history; Bluetooth through `bluer` | always running |
| macOS | SwiftUI menu bar app with no Dock icon; Bluetooth through CoreBluetooth | login item through `SMAppService.mainApp`; an `NSProcessInfo` activity keeps App Nap from slowing it |
| Android | Compose app | foreground service of type `connectedDevice` (`dataSync` is capped at 6 hours a day); Companion Device Manager restarts it when a paired desktop comes into Bluetooth range |
| iOS | app, Share Extension, Control Center control, Shortcuts actions, optional keyboard, Local Push extension if approved | can't stay running, but Bluetooth wakes it near a desktop; see [iOS](#ios) |

The iOS and macOS apps share one Swift package and one XCFramework build of the core.

### Repo layout
| Path | What's there |
|---|---|
| `core/` | the Rust core and its UniFFI bindings |
| `linux/` | the background service, tray icon and pairing/history window |
| `apple/` | the shared Swift package, the core XCFramework, the iOS app and its extensions, the macOS menu bar app |
| `android/` | the Android app |
| `relay/` | config for our `iroh-relay` (accounts phase) |
| `docs/protocol.md` | the wire protocol |

## 4. What each platform allows

| | Notices a copy by itself | Puts incoming items on the clipboard by itself |
|---|---|---|
| noobsbot-desktop | ✅ once the compositor exposes data-control | ✅ |
| macOS | ✅ checks the clipboard every 0.5 s. If Apple turns on its clipboard-read alert, the user allows it once | ✅ |
| Android | ⚠️ one tap by default; automatic in power-user mode (Shizuku) | ✅ through the foreground service |
| iOS 27 | ❌ apps are paused in the background; sending takes one tap | ⚠️ one tap. A notification arrives in the background near a desktop over Bluetooth, or on home Wi-Fi with Local Push (Apple must approve it) |

## 5. Pairing and trust
- On first run, each device creates its own key pair; its public key is its iroh address. The private key is stored
  in the Keychain on Apple devices, the Android Keystore, or Secret Service on Linux.
- **To pair,** the desktop shows a QR code with its public key, its local addresses and a one-time secret. The phone
  scans it, connects, and proves it knows the secret. Both devices then save each other's key. Two desktops without a
  camera (Mac and Linux) each show a 6-digit code instead, and you confirm they match.
- **Pairing with one device also pairs you with the devices it already trusts.** Four devices then take 3 pairings
  instead of 6. Removing a device on any device tells the others.
- Connections from keys that aren't paired are dropped. No certificate authority or server is involved.
- Bluetooth uses the same keys and our own end-to-end encryption. Bluetooth pairing isn't needed (see
  [Bluetooth](#bluetooth)).

## 6. Clipboard content
An item has an id, the device it came from, a timestamp, a list of formats, and a "sensitive" flag.

| Format sent | Linux (Wayland) | macOS / iOS | Android |
|---|---|---|---|
| UTF-8 text | `text/plain;charset=utf-8`, `UTF8_STRING` | `public.utf8-plain-text` | ClipData text |
| HTML | `text/html` | `public.html` | `htmlText` |
| PNG | `image/png` | `public.png` (TIFF/HEIC converted) | content:// URI |
| JPEG (sent unchanged) | `image/jpeg` | `public.jpeg` | content:// URI |
| Files | `text/uri-list` + `x-special/gnome-copied-files` pointing at local copies | file URLs | FileProvider URIs |

- **Skipped:** RTF and formats specific to one app (Office or Figma internals); these stay on the device they were
  copied on.
- **Never sent:** items marked as concealed or transient on Apple devices (`org.nspasteboard.ConcealedType`,
  `org.nspasteboard.TransientType`), sensitive on Android (`EXTRA_IS_SENSITIVE`), or as a secret on Wayland
  (`x-kde-passwordManagerHint: secret`).
- **No echo:** when a device writes an incoming item to its clipboard, its own watcher sees the change. It ignores any
  change whose content hash matches the item it just wrote.
- **Apple's Universal Clipboard:** when a Mac and an iPhone share an Apple ID, Apple also syncs between them, and
  there's no API to turn that off. To avoid duplicates:
  - The Mac writes incoming items as `.currentHostOnly`, so Apple doesn't forward them.
  - It skips items marked `com.apple.is-remote-clipboard`, which came from Apple's sync.
  - The hash check catches anything left.
- **No conflict handling:** the newest copy wins, and everything goes into history.

## 7. Transfer
How an item is sent depends on its size and the connection. The size cap defaults to 25 MB and can be changed.
- **Text, HTML and links** are pushed to every online device right away, over any connection.
- **Images and files under the cap** are pushed right away over the same network. Over mobile data they follow the
  [mobile data](#mobile-data) rules. Over Bluetooth, only items up to about 1 MB go; bigger ones wait for a network.
- **Anything over the cap** is only announced, for example "video.mov, 1.2 GB from MacBook". You tap Download in the
  notification or history, and when it arrives it's put on the clipboard.
- Files are saved to a cache folder and the clipboard points at those local copies.
- A device that comes back online gets only the latest item, not everything it missed.
- Files are checked by hash. If iroh-blobs is stable at 1.x, use it for files: it checks data as it streams and can
  resume a transfer. Otherwise a failed transfer starts over.

### Wire format
- Each device keeps one iroh connection to each paired device that's online. Every device connects to every other;
  that's fine for up to about 10 devices.
- One control stream carries small messages. Each item's data goes on its own stream, so a large file never holds up
  text.
- Messages are a small JSON header with a raw binary body. The first message (`Hello`) carries the protocol version
  and features, so devices on different app versions fall back to what both support instead of failing.
- Messages: `Hello`, `Offer`, `Request`, `Data`, `Ack`, `PeerIntro`, `Unpair`. Keepalives come from QUIC, so there's no
  `Ping`.

## 8. Per platform

### noobsbot-desktop (Linux)
- A background service (systemd user service) with a tray icon and a small iced window for pairing and history, in
  the same style as System Settings.
- This is the one change needed in the compositor: expose `ext-data-control-v1` only to clients on the trusted
  socket, which fits the existing privacy model.
- The compositor keeps its own 10 MB copy of the clipboard. Check that this still works when our tool sets the
  clipboard through data-control.
- Watching and setting the clipboard uses the `wl-clipboard-rs` crate (0.9.5 supports `ext-data-control` and has a
  watcher), so we don't write the protocol code ourselves.
- **Other desktops:** KDE (KWin), Sway, Hyprland, COSMIC and niri support data-control, so the Linux app works there
  too. GNOME doesn't: Mutter 51 has no support and the request was closed.
- **What to watch for:**
  - Data-control doesn't need keyboard focus.
  - Syncing the primary selection (select-to-paste) is off by default, because it changes on every text selection.
  - Read the clipboard asynchronously with a size limit, and keep answering paste requests while doing so.
  - Ignore our own changes when they come back.
- The install docs need to say which UDP port to open in the firewall.

### macOS
macOS 27 is current.
- **Background app:** a SwiftUI menu bar app with no Dock icon (`LSUIElement`), started at login through
  `SMAppService.mainApp`. Don't use a LaunchAgent: a LaunchAgent gets no Local Network permission.
- **App Nap:** it slows timers, priority and I/O in background apps. An `NSProcessInfo` activity keeps it off. Give the
  0.5 s timer about 10% tolerance so the system can group wakeups. How App Nap affects open connections hasn't been
  confirmed; test it in Phase 0.
- **Detecting copies:** check the clipboard's `changeCount` every 0.5 s. macOS has no clipboard-change event; Maccy
  does the same.
- **Clipboard read alerts:**
  - Apple has announced alerts when an app reads the clipboard by itself, still listed as "upcoming". Whether
    macOS 26 or 27 shows them by default isn't confirmed.
  - After the first alert, the user can choose "Always Allow" in System Settings. No API asks for it directly.
  - The app checks `NSPasteboard.accessBehavior` and walks the user to that setting if needed.
- **Writing large items:** clipboard managers read everything as soon as it changes, so data supplied only on request
  doesn't save work. Finder won't paste file promises (those are for drag and drop), so files are downloaded first,
  then written as file URLs.
- **Local Network prompt:** it appears the first time the app uses Bonjour or connects to a local address, not when it
  accepts a connection. The first attempt may fail while the prompt is showing; retry after it's answered. The app
  needs `NSLocalNetworkUsageDescription`, `NSBonjourServices` and Apple code signing.
- **Distribution:** a notarized Developer ID build first. A Mac App Store build would also need the sandbox's
  `network.server` and `network.client` entitlements.

### Android
Android 17 is current. Staying connected and receiving both work in the background. Reading the clipboard in the
background doesn't. Since Android 10, only these can read it: the default keyboard, the app in focus, and apps with
`READ_CLIPBOARD_IN_BACKGROUND` (system apps and the `shell` user). Android 17 hasn't changed that.

- **Service:**
  - A foreground service of type `connectedDevice`, with an ongoing notification ("Connected to 2 devices").
  - This type has no time limit, and KDE Connect uses it. `dataSync` is capped at 6 hours a day.
  - Play needs a declaration and a video for this service type.
- **Permissions:**
  - `ACCESS_LOCAL_NETWORK`, required when targeting Android 17
  - `BLUETOOTH_SCAN` (with `neverForLocation`), `BLUETOOTH_CONNECT` and `BLUETOOTH_ADVERTISE`
  - notifications
- **Staying connected:**
  - A foreground service keeps network access during Doze.
  - The desktop sends keepalive pings, so the phone doesn't have to wake up to send its own.
  - The app reconnects when `NetworkCallback` reports a network change.
  - No Wi-Fi lock. The app holds a multicast lock only while looking for devices.
  - Sockets are tied to the Wi-Fi network (see [Same network without internet](#same-network-without-internet)).
  - First-run setup walks users through turning off battery restrictions on Xiaomi, Samsung, Huawei and other phones
    (see dontkillmyapp.com).
- **Bluetooth (Android 12+):**
  - The app makes a Companion Device Manager association with each desktop.
  - When the desktop comes into range, Android starts our `CompanionDeviceService`, which starts the foreground
    service from the background.
  - Whether this works with Apple devices isn't confirmed, because they change their Bluetooth address often.
- **Receiving:** automatic. Apps may write the clipboard from the background.
  - On Android 13+ the system shows its "copied" overlay for every write, and we can't hide it.
  - Incoming sensitive items keep `EXTRA_IS_SENSITIVE`.
- **Sending, default (one tap, allowed on Play):** automatic while our app is open. Otherwise:
  - the Quick Settings tile, a notification button or a bubble. Each opens a see-through activity for a moment to get
    focus, read the clipboard and send it.
  - the share sheet
  - "Send to devices" in the text selection menu (`PROCESS_TEXT`)
- **Sending, power-user mode (automatic):**
  - A Shizuku service runs our clipboard listener as the `shell` user, which may read the clipboard in the
    background. Shizuku has to be restarted after each reboot.
  - The fallback is the `READ_LOGS` trick that KDE Connect and ClipCascade use. After a one-time ADB grant, the app
    watches the system log for clipboard changes and opens a see-through activity to read the clipboard.
  - On Android 13+ a log-access dialog appears when the log reader starts. KDE Connect ships this trick on Play.
- **Not used:**
  - Accessibility services: they miss copies, need a Play declaration form, and are turned off by Android 17's
    Advanced Protection mode.
  - Our own keyboard: only worth it if we build a real one.
  - `appops`: has no effect, because the focus check comes first.
  - Root or Xposed: documented only.
- Received files go into Downloads through MediaStore.

### iOS
iOS 27 is current. No app can keep a connection or read the clipboard in the background, so each direction needs at
least one tap. KDE Connect, Paste and LocalSend have the same limits.

| How | What it does | Speed |
|---|---|---|
| Opening the app | Connects over Bonjour. Sends the current clipboard and pulls the latest item | Instant |
| Control Center control, Back Tap or Action Button | Opens the app, which reads the clipboard and sends it | One tap, about 1–2 s |
| Share Extension | Sends selected images, files or text | While the share sheet is open |
| Bluetooth near a desktop | The app keeps a Bluetooth connection open to the Mac or Linux machine. When a new item arrives, iOS wakes the app for about 10 s and it shows a notification with a preview. Tapping it puts the item on the clipboard | About 1 s once connected |
| Local Push (needs Apple's approval) | A small extension keeps a connection to a desktop on your home Wi-Fi and shows a notification with a preview. Tapping it puts the item on the clipboard | About 1 s to the notification |
| Keyboard (optional, needs Full Access) | Shows the latest items and inserts text into the field you're typing in (not password fields) | While the keyboard is on screen |
| Large transfers | `BGContinuedProcessingTask` (iOS 26+) keeps a transfer you started going after you leave the app | Background |

- **Setup:** accept the Local Network prompt (it only appears while the app is open), and set "Paste from Other Apps"
  to Allow so reads don't prompt. `UIPasteControl` also reads without a prompt.
- **Local Push:** it runs only on Wi-Fi networks you list, plus Ethernet. It doesn't run on public cellular. It needs
  an entitlement requested from Apple: reported waits are 2 days to 4 weeks, and approval for clipboard use isn't
  confirmed. Apply on day one and ship without it. Without it, Bluetooth covers receiving near a desktop, and
  otherwise the iPhone pulls the latest item when the app opens.
  - Test in Phase 0 whether iroh fits inside the extension's memory limit. If not, the extension uses a small TLS
    connection with the same key.
  - On iOS 26 there's a reported bug where a full-tunnel VPN stops the extension.
- **Bluetooth wake-ups:**
  - The app keeps a Bluetooth connection request to a desktop open (it never times out) and uses state restoration.
  - iOS relaunches the app after it was closed for memory, crashed, or the phone restarted. It doesn't after the user
    force-quits the app or turns Bluetooth off and on.
  - Whether the app can write the clipboard while woken isn't confirmed, so the notification is the safe path.
  - App Review risk is moderate. The Bluetooth background mode must back a visible Bluetooth feature; syncing with a
    nearby computer over Bluetooth is that feature.
  - iPhone to iPhone over Bluetooth in the background is unreliable. A background iPhone's advertising can only be
    seen by iPhones scanning for our service.
- **Ruled out:**
  - VPN tunnel: guideline 5.4 and TN3120. It also takes the VPN slot.
  - Audio, location or VoIP keep-alives: guideline 2.5.4, and PushKit needs a CallKit call for every push.
- **Accounts phase:** push notifications through Apple's service (APNs) make receiving work anywhere, not just on home
  Wi-Fi.
- The app has to declare why it uses the local network and which Bonjour service it looks for
  (`NSLocalNetworkUsageDescription`, `NSBonjourServices`).

## 9. Phases
0. **Short tests first (a few days):**
   - iroh on iOS and Android: binary size, battery, and connecting with addresses from Bonjour/NSD
   - data-control in noobsbot-desktop
   - Android power-user mode with Shizuku
   - Bluetooth: waking the iPhone app near a desktop, L2CAP speed on real devices, and Companion Device Manager with
     Apple devices
   - macOS: App Nap's effect on open connections
   - **Request the iOS Local Push entitlement now.**
1. **Core (iroh, pairing, protocol), Linux and macOS, text only, same network, and the CI version and release
   workflows from `VERSIONING.md`.** Done when text copied on the Mac
   shows up on noobsbot-desktop within 0.5 s, and the same in the other direction, with and without internet on the
   Wi-Fi.
2. **Android:** the background service, receiving on its own, sending (tile, notification, share sheet, text menu),
   sockets tied to Wi-Fi, and the Shizuku power-user mode.
3. **iOS:** sync when the app opens, the Control Center control, the Share Extension, and Local Push if Apple has
   approved it.
4. **Images, files, size limits and mobile data rules.**
5. **Bluetooth:**
   - finding and waking devices, including the iPhone app
   - sending small items when there's no shared network
   - handing larger items over to the network when one is available
   - AWDL and Wi-Fi Aware come later
6. **History with search, per-device rules, sending to one device, pause and notifications.**
7. **Accounts:** sign-in, our own `iroh-relay` in two regions, APNs/FCM push, and removing devices for the whole
   account.

## 10. Accounts phase (later)
- Each device's identity is already a key pair. An account just records which keys belong to it, and the pairing list
  becomes the account's device list.
- iroh's API for connecting to a device by its key stays the same; relays are turned on. Our relay only ever sees
  encrypted QUIC, so encryption stays end to end.
- Push notifications through Apple's and Google's services (APNs/FCM) let iOS receive anywhere, and wake Android when
  its battery saver has cut the connection.
- On the same network, devices still connect directly and never go through the relay.
- None of this is built before this phase.

## 11. Risks
| Risk | Effect | What we do |
|---|---|---|
| Apple doesn't approve Local Push | iOS receives only when the app is opened | Ship without it; APNs in the accounts phase |
| iOS limits in general | iOS is never fully automatic | Make the single tap as fast as possible (Control Center, Back Tap, notification) |
| n0 controls iroh | Changes to the API, or the default relays going away | Pin to 1.x, run our own relay |
| Android blocks reading the clipboard in the background | Android sends need a tap | Power-user mode with Shizuku, or the `READ_LOGS` trick |
| Phone makers' battery savers stop the Android service | Android stops receiving | First-run guide; Companion Device Manager restarts the service near a desktop |
| Bluetooth is slow | Large items can't go over Bluetooth | Bluetooth carries items up to about 1 MB and hands larger ones to the network |
| App Review questions the Bluetooth background mode | The iPhone loses Bluetooth wake-ups | Bluetooth is a visible feature (syncing with a nearby computer); Local Push and syncing when the app opens still work |
| noobsbot-desktop needs data-control | No automatic copy detection on Linux until it's added | Small compositor change, trusted clients only |
| GNOME has no data-control | The Linux app can't watch the clipboard on GNOME | Not a target; KDE, Sway, Hyprland, COSMIC and niri work |
| Apple turns on clipboard read alerts | macOS asks before our app reads the clipboard | First-run setup walks the user to "Always Allow" |
| Wi-Fi that blocks devices from reaching each other | No direct connection | Bluetooth or the internet |

## 12. Versioning and releases
We follow [`VERSIONING.md`](VERSIONING.md): Semantic Versioning set by CI from commit types, betas on every fix,
releases on `feat:` or once a day, and nobody sets a version by hand. What this repo adds:

- **One version for all apps.** The Linux, macOS, Android and iOS apps share the core and the protocol, so one tag
  (`v1.2.0`) covers all of them.
- **`.github/set-version.sh` writes the version everywhere:**
  - `Cargo.toml` and `Cargo.lock` (core and Linux)
  - Android `versionName` and `versionCode`
  - Apple `MARKETING_VERSION` and `CURRENT_PROJECT_VERSION`
- **Build numbers for the app stores.** Both stores need a whole number that always goes up, made from the version:
  - The formula is `MAJOR×1,000,000 + MINOR×10,000 + PATCH×100 + N`, where N is the beta number and 99 for a
    release.
  - So `1.0.1-beta.1` → 1000101, `1.0.1-beta.2` → 1000102, `1.0.1` → 1000199, `1.1.0` → 1010099. This keeps the
    order in `VERSIONING.md` and allows up to 98 betas per version.
- **Where builds go:**

  | | Beta | Release |
  |---|---|---|
  | Linux | tarball, GitHub pre-release | tarball, GitHub release |
  | macOS | notarized build, GitHub pre-release | notarized build, GitHub release |
  | Android | APK on GitHub pre-release (and Play internal testing) | APK on GitHub (and Play Store) |
  | iOS | TestFlight | App Store |

- **Store releases lag behind tags.** App Store and Play Store review takes time, and users update at different
  times, so devices on different app versions will talk to each other. That's why the protocol has its own version
  in `Hello`. A newer app still talks to the previous protocol version.
- **What counts as breaking (`feat!:`)**, adding to `VERSIONING.md`'s list:
  - a protocol change that older apps can't talk to
  - a change that needs devices to be paired again
  - a change to `docs/protocol.md` that removes a message or field
- **Signing:** the release signing keys stay in CI secrets, as `VERSIONING.md` says. That covers the Linux tarball's
  signature, Apple Developer ID and notarization, and the Android signing key.
- **Store submissions are by hand** until [Open decisions](#open-decisions) item 4 is settled. CI uploads every build to
  TestFlight and Play internal testing; the owner sends releases to review.

## Open decisions
1. **Name**, which is also used for the Bonjour service name and the app IDs.
2. **Distribution:** App Store and Play Store, or direct installs (and F-Droid) first? Either way, installing on a
   real iPhone for more than 7 days needs a paid Apple developer account. The Local Push entitlement also needs an
   Apple developer account.
3. **Minimum versions:** suggested Android 10+, iOS 17+ (needed for interactive widgets; Control Center controls need
   iOS 18) and macOS 14+.
4. **App Store cadence:** `VERSIONING.md` makes a release every day there are betas. Sending iOS builds to App Review
   every day is too often. Should store uploads happen for every release, or once a week with the newest release?

## Sources
- iroh releases and 1.0: https://github.com/n0-computer/iroh/releases, https://iroh.computer/blog/v1
- iroh relays and rate limits: https://docs.iroh.computer/relays/rate-limiting
- iroh language bindings: https://iroh.computer/blog/iroh-language-support
- Android foreground service limits: https://developer.android.com/about/versions/15/behavior-changes-15
- Android 17 local network permission: https://developer.android.com/privacy-and-security/local-network-permission
- iOS multicast entitlement: https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.networking.multicast
- iOS Local Push Connectivity: https://developer.apple.com/documentation/networkextension/local-push-connectivity
- iOS local network privacy: https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy
- VPN tunnel use cases: https://developer.apple.com/documentation/technotes/tn3120-expected-use-cases-for-network-extension-packet-tunnel-providers
- App Review guidelines: https://developer.apple.com/app-store/review/guidelines/
- Continued processing tasks: https://developer.apple.com/documentation/backgroundtasks/bgcontinuedprocessingtask
- Long-running intents: https://developer.apple.com/documentation/appintents/longrunningintent
- Paste permission: https://pasteapp.io/help/allow-paste-permissions-in-ios
- KDE Connect on iOS: https://discuss.kde.org/t/kde-connect-can-share-clipboard-from-pc-to-iphone-but-not-vice-versa/28231
- macOS clipboard privacy: https://developer.apple.com/documentation/updates/appkit#macOS-pasteboard-privacy
- macOS clipboard access setting: https://developer.apple.com/documentation/appkit/nspasteboard/accessbehavior-swift.enum
- Maccy (clipboard polling, skipping Universal Clipboard copies): https://github.com/p0deje/Maccy
- App Nap: https://developer.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/AppNap.html
- Wayland data-control support: https://wayland.app/protocols/ext-data-control-v1
- wl-clipboard-rs: https://github.com/YaLTeR/wl-clipboard-rs/blob/master/CHANGELOG.md
- Android foreground service types: https://developer.android.com/develop/background-work/services/fgs/service-types
- Play foreground service declaration: https://support.google.com/googleplay/android-developer/answer/13392821
- Android clipboard access rules: https://android.googlesource.com/platform/frameworks/base/+/main/services/core/java/com/android/server/clipboard/ClipboardService.java
- KDE Connect clipboard listener: https://invent.kde.org/network/kdeconnect-android/-/blob/master/src/main/java/org/kde/kdeconnect/plugins/clipboard/ClipboardListener.kt
- ClipCascade: https://github.com/Sathvik-Rao/ClipCascade
- Shizuku setup: https://shizuku.rikka.app/guide/setup/
- Phone makers' battery savers: https://dontkillmyapp.com
- Companion Device Manager: https://developer.android.com/develop/connectivity/bluetooth/companion-device-pairing
- Android Bluetooth permissions: https://developer.android.com/develop/connectivity/bluetooth/bt-permissions
- Core Bluetooth in the background (iOS): https://developer.apple.com/library/archive/documentation/NetworkingInternetWeb/Conceptual/CoreBluetooth_concepts/CoreBluetoothBackgroundProcessingForIOSApps/PerformingTasksWhileYourAppIsInTheBackground.html
- Bluetooth state restoration: https://developer.apple.com/library/archive/qa/qa1962/_index.html
- iroh over Bluetooth: https://docs.iroh.computer/transports/bluetooth.md
- bluer (Linux Bluetooth): https://docs.rs/bluer
- Wi-Fi Aware on iOS: https://developer.apple.com/documentation/wifiaware
- Handoff security: https://support.apple.com/guide/security/handoff-security-secf78dbe639/web
- Bitchat (Noise over Bluetooth): https://github.com/permissionlesstech/bitchat
