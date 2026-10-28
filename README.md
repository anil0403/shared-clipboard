# Shared Clipboard

Copy on one device, paste on another. Works with noobsbot-desktop (Linux, Wayland), macOS, Android and iOS, over the
same network (with or without internet). Bluetooth and the internet (with accounts) come later.

"Shared Clipboard" is a working name.

**Status:** in development; nothing is released yet. [`plan.md`](plan.md) has the design and the phases.

- **Core and Linux app:** work, with tests between real nodes and an end-to-end check of two paired daemons
  ([`linux/e2e.sh`](linux/e2e.sh)).
- **Android app:** builds; not yet run on a phone.
- **macOS and iOS apps:** written; compiled only by CI on a Mac.
- **Bluetooth and accounts:** not started.

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
