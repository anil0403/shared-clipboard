# Shared Clipboard

Copy on one device, paste on another. Works with noobsbot-desktop (Linux, Wayland), macOS, Android and iOS, over the
same network (with or without internet). Bluetooth and the internet (with accounts) come later.

"Shared Clipboard" is a working name.

**Status:** in development; nothing is released yet. [`plan.md`](plan.md) has the design and the phases.

## Layout

| Path | What's there |
|---|---|
| `core/` | the Rust core every app uses: identity, pairing, protocol, transfers ([`docs/protocol.md`](docs/protocol.md)) |
| `linux/` | the Linux app |
| `android/` | the Android app |
| `apple/` | the macOS and iOS apps |
| `docs/` | the wire protocol and other docs |

## Versions

Versions and releases are made by CI from commit types: see [`VERSIONING.md`](VERSIONING.md).
