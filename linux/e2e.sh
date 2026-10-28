#!/bin/bash
# End-to-end: two headless KWins, two `sharedclip daemon`s paired by link; copy on one, paste on the other.
# Needs kwin_wayland, dbus-run-session, wl-clipboard and ImageMagick. Never touches your session.
#   cargo build -p sharedclip-linux && bash linux/e2e.sh [sharedclip binary] [work dir]
BIN=${1:-$PWD/target/debug/sharedclip}; W=${2:-$PWD/target/e2e}
RA=/run/user/$(id -u)/sc-e2e-a; RB=/run/user/$(id -u)/sc-e2e-b; SOCK=wayland-sce2e
rm -rf "$W" "$RA" "$RB"; mkdir -p "$W/a" "$W/b"; mkdir -m 700 "$RA" "$RB"
unset DISPLAY WAYLAND_SOCKET
FAILED=0
NAME=$(cat /proc/sys/kernel/hostname) # both daemons are named after it
cleanup() {
  kill -TERM "${DAEMONS[@]}" 2>/dev/null
  for p in "${BUSES[@]}"; do pkill -TERM -P "$p"; done # each KWin and its D-Bus
  wait
  rm -rf "$RA" "$RB"
}
trap cleanup EXIT
# Runs a command on side a or b: its own runtime dir, compositor and data dir.
on() {
  local r=$RA d=$W/a; [ "$1" = b ] && r=$RB d=$W/b; shift
  env XDG_RUNTIME_DIR="$r" WAYLAND_DISPLAY=$SOCK XDG_DATA_HOME="$d/data" XDG_CONFIG_HOME="$d/config" \
    XDG_CACHE_HOME="$d/cache" "$@"
}
check() { if [ "$2" = "$3" ]; then echo "ok: $1"; else echo "FAIL: $1: expected [$2], got [$3]"; FAILED=1; fi; }

BUSES=()
for s in a b; do
  on $s dbus-run-session -- kwin_wayland --virtual --no-lockscreen --no-global-shortcuts --socket $SOCK \
    --width 800 --height 600 > "$W/$s/kwin.log" 2>&1 &
  BUSES+=($!)
done
for _ in $(seq 50); do [ -S "$RA/$SOCK" ] && [ -S "$RB/$SOCK" ] && break; sleep 0.2; done
DAEMONS=()
for s in a b; do on $s "$BIN" daemon > "$W/$s/daemon.log" 2>&1 & DAEMONS+=($!); done
for _ in $(seq 50); do [ -S "$RA/sharedclip.sock" ] && [ -S "$RB/sharedclip.sock" ] && break; sleep 0.2; done
sleep 3
cat "$W/a/daemon.log" "$W/b/daemon.log"
check "control socket is 0600" 600 "$(stat -c %a "$RA/sharedclip.sock")"
echo "pair --nearby on B (mDNS):"; on b "$BIN" pair --nearby

on a timeout 30 "$BIN" pair < /dev/null > "$W/a/pair.out" 2>&1 &
PAIR=$!
for _ in $(seq 25); do grep -q '^sharedclip://' "$W/a/pair.out" && break; sleep 0.2; done
check "B pairs by A's link" "Paired with $NAME" "$(on b "$BIN" pair "$(grep -m1 '^sharedclip://' "$W/a/pair.out")")"
wait $PAIR
check "A's pair session sees it" "Paired with $NAME" "$(tail -1 "$W/a/pair.out")"
sleep 1

on a wl-copy "hello from A" > /dev/null 2>&1; sleep 2
check "text A -> B" "hello from A" "$(on b wl-paste -n)"
on b wl-copy "hello from B" > /dev/null 2>&1; sleep 2
check "text B -> A" "hello from B" "$(on a wl-paste -n)"

magick -size 64x48 gradient:red-blue "$W/a/pic.png"
on a wl-copy -t image/png < "$W/a/pic.png" > /dev/null 2>&1; sleep 2
on b wl-paste -t image/png > "$W/b/pic.png"
check "PNG A -> B" same "$(cmp -s "$W/a/pic.png" "$W/b/pic.png" && echo same)"

printf 'file body from A\n' > "$W/a/notes a.txt"
SENT=$(python3 -c 'import sys, urllib.parse as u; print("file://" + u.quote(sys.argv[1]))' "$W/a/notes a.txt")
on a wl-copy -t text/uri-list "$SENT"$'\r\n' > /dev/null 2>&1; sleep 2
URI=$(on b wl-paste -n -t text/uri-list | head -1 | tr -d '\r')
FILE=$(python3 -c 'import sys, urllib.parse as u; print(u.unquote(sys.argv[1][7:]))' "$URI")
check "file A -> B, saved on B" "file body from A" "$(cat "$FILE")"
check "gnome-copied-files on B" "copy" "$(on b wl-paste -n -t x-special/gnome-copied-files | head -1)"

echo "history on B:"; on b "$BIN" history
on b "$BIN" pause; on a wl-copy "while paused" > /dev/null 2>&1; sleep 2
check "paused B gets nothing" "$URI" "$(on b wl-paste -n -t text/uri-list | head -1 | tr -d '\r')"
on b "$BIN" resume
on b "$BIN" unpair "$NAME"; sleep 1
check "unpair reaches A" "No paired devices. Pair one with: sharedclip pair" "$(on a "$BIN" devices)"

# Pair again by code: B picks A from the nearby list, both see the code and accept.
printf 'y\n' | on a timeout 30 "$BIN" pair > "$W/a/code.out" 2>&1 &
PAIR=$!
sleep 1
printf 'y\n' | on b timeout 30 "$BIN" pair --nearby 1 > "$W/b/code.out" 2>&1
wait $PAIR
grep -o 'Pairing code with .*' "$W/a/code.out" "$W/b/code.out"
check "code pairing on B" "Paired with $NAME" "$(grep -o "Paired with .*" "$W/b/code.out")"
check "code pairing on A" "Paired with $NAME" "$(grep -o "Paired with .*" "$W/a/code.out")"
[ $FAILED = 0 ] && echo "all passed" || echo "some checks FAILED"
exit $FAILED
