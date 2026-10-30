# Shared Clipboard

Copy on one device, paste on another. Works with noobsbot-desktop and other Linux desktops (Wayland or X11), macOS, Android and iOS, over the
same network (with or without internet). Bluetooth and the internet (with accounts) come later.

"Shared Clipboard" is a working name.

**Status:** in development; nothing is released yet. [`plan.md`](plan.md) has the design and the phases.

- **Core and Linux app:** work, with tests between real nodes and an end-to-end check of two paired daemons
  ([`linux/e2e.sh`](linux/e2e.sh)).
- **Android app:** builds; not yet run on a phone.
- **macOS and iOS apps:** written; compiled only by CI on a Mac.
- **Bluetooth and accounts:** not started.

## Install

From [Releases](https://github.com/noobsbot-team/shared-clipboard/releases):

- **Linux:** one command installs the latest release, checks its signature and starts the service. Run it again to
  update.
  ```sh
  curl -fsSL https://raw.githubusercontent.com/noobsbot-team/shared-clipboard/main/install.sh | sh
  ```
  It installs `~/.local/bin/sharedclip` and the systemd user service. To remove it:
  `systemctl --user disable --now sharedclip && rm ~/.local/bin/sharedclip ~/.config/systemd/user/sharedclip.service`.
  Or by hand: unpack `sharedclip-<version>-x86_64-linux.tar.gz` and copy those two files to the same places.
- **Android:** `sharedclip-<version>-android.apk`. Open it on the phone and allow installing from that app when
  asked. A newer APK installs over it and keeps the paired devices.

Every file is signed with [minisign](https://jedisct1.github.io/minisign/). To check one:
`minisign -Vm <file> -p minisign.pub` (the key is [`minisign.pub`](minisign.pub)). `SHA256SUMS` lists their
checksums.

## Layout

| Path | What's there |
|---|---|
| `core/` | the Rust core every app uses: identity, pairing, protocol, transfers ([`docs/protocol.md`](docs/protocol.md)) |
| `linux/` | the Linux app: a daemon and the `sharedclip` command ([README](linux/README.md)) |
| `android/` | the Android app ([README](android/README.md)) |
| `apple/` | the macOS and iOS apps ([README](apple/README.md)) |
| `docs/` | the wire protocol and other docs |
| `.github/` | CI, versioning and release workflows |

## Versions

Versions and releases are made by CI from commit types: see [`VERSIONING.md`](VERSIONING.md).
