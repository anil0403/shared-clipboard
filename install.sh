#!/bin/sh
# Installs or updates Shared Clipboard for Linux from the latest GitHub release, checks its signature, and starts the
# service. Run it again to update. README.md has the one-line command.
set -eu
repo=anil0403/shared-clipboard
# minisign.pub in the repository.
key=RWRo2xOK8e1+BWY1HYBQBMQdef+gOvhMYXOiZq3TKpBk6RCcNSY6veNK
bin=$HOME/.local/bin
units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user

say() { printf 'sharedclip: %s\n' "$*"; }
die() { say "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "this installs the Linux app only"
[ "$(uname -m)" = x86_64 ] || die "there are only x86_64 builds for now"

# A private repository can only be read through the GitHub CLI, signed in.
if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
  tag=$(gh release view -R "$repo" --json tagName -q .tagName)
else
  command -v curl >/dev/null || die "needs curl"
  tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
fi
[ -n "$tag" ] || die "can't find the latest release (a private repository needs the GitHub CLI: gh auth login)"
version=${tag#v}
name=sharedclip-$version-x86_64-linux

if [ "$("$bin/sharedclip" --version 2>/dev/null || true)" = "sharedclip $version" ]; then
  say "$version is already installed"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
say "downloading $version"
if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
  gh release download "$tag" -R "$repo" -p "$name.tar.gz" -p "$name.tar.gz.minisig" -p SHA256SUMS
else
  for file in "$name.tar.gz" "$name.tar.gz.minisig" SHA256SUMS; do
    curl -fsSLO "https://github.com/$repo/releases/download/$tag/$file"
  done
fi

grep " $name.tar.gz\$" SHA256SUMS | sha256sum -c --quiet - || die "the download is damaged"
if command -v minisign >/dev/null; then
  minisign -Vqm "$name.tar.gz" -P "$key" || die "the signature doesn't match: not installing"
else
  say "minisign isn't installed, so only the checksum was checked"
fi

tar -xzf "$name.tar.gz"
mkdir -p "$bin" "$units"
# Through a new file and a rename, so a running daemon doesn't stop us ("text file busy").
install -m755 "$name/sharedclip" "$bin/.sharedclip.new"
mv -f "$bin/.sharedclip.new" "$bin/sharedclip"
install -m644 "$name/sharedclip.service" "$units/sharedclip.service"

if systemctl --user show-environment >/dev/null 2>&1; then
  systemctl --user daemon-reload
  systemctl --user enable --quiet sharedclip
  systemctl --user restart sharedclip
  say "installed $version and started it (logs: journalctl --user -u sharedclip)"
else
  say "installed $version; start it with: sharedclip daemon"
fi
case ":$PATH:" in
*":$bin:"*) ;;
*) say "add $bin to your PATH to run sharedclip" ;;
esac
say "pair a device with: sharedclip pair"
