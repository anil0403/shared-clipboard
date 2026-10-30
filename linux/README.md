# Shared Clipboard for Linux

A background service that syncs the clipboard (text, HTML, PNG images and files) with your other devices on the same
network. On X11 it works as is. On Wayland it needs a compositor with
`ext-data-control-v1` or `wlr-data-control`: noobsbot-desktop, KDE, Sway, Hyprland, COSMIC and niri have it; GNOME
and Cinnamon's Wayland session don't. It picks by what's running, so one install serves both kinds of session.

## Install
A release has `sharedclip` and `sharedclip.service` ready to install: see the [main README](../README.md#install).
To build it yourself:
```sh
cargo build --release -p sharedclip-linux
install -Dm755 target/release/sharedclip ~/.local/bin/sharedclip
install -Dm644 linux/sharedclip.service ~/.config/systemd/user/sharedclip.service
systemctl --user enable --now sharedclip
```
Logs: `journalctl --user -u sharedclip`. Devices connect over UDP port 47820; open it if you run a firewall
(`sudo ufw allow 47820/udp`), and allow mDNS (UDP 5353) so devices find each other.

## Pair
- `sharedclip pair` shows a link and a QR code, then waits. Scan it on a phone, or run `sharedclip pair <link>` on
  another computer.
- `sharedclip pair --nearby` lists devices found on the network; `sharedclip pair --nearby <n>` pairs with one: both
  show a 6-digit code, and you accept on each if they match.
- `sharedclip devices`, `unpair <id or name>`, `history`, `pause` and `resume` do what they say.

## Bluetooth
When a paired phone can't reach this computer over the network, it can still sync over Bluetooth LE: the service
advertises itself and the phone connects to it. Text and small items (up to 1 MiB) go over Bluetooth; bigger ones
and files wait for the network. For this:
- the computer needs a Bluetooth adapter and BlueZ (`bluetoothd`), and Bluetooth must be turned on. The service
  never turns it on; it waits, and starts as soon as you turn Bluetooth on;
- the phone must be paired first, over the network (see Pair). There's no Bluetooth pairing.

To leave Bluetooth out, run the service with `SHAREDCLIP_NO_BLUETOOTH=1`.

## noobsbot-desktop
noobsbot-desktop offers clipboard control only on its trusted socket (`$WAYLAND_DISPLAY-trusted`). The service
connects there by itself when the socket exists; nothing else is needed. Run by hand, `sharedclip daemon` works as
is, like `noobsbot-desktop --trusted sharedclip daemon`.

## Tests
`cargo test -p sharedclip-linux` runs the unit tests. With a compositor that has data-control,
`cargo test -p sharedclip-linux live -- --ignored --skip x11` checks the clipboard for real, and `bash linux/e2e.sh`
pairs two daemons on two headless KWins and copies text, an image and a file between them. For X11, with Xvfb and
xclip: `xvfb-run -a cargo test -p sharedclip-linux x11 -- --ignored`, and `E2E_X11=1 bash linux/e2e.sh`.
