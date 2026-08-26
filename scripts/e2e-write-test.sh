#!/bin/zsh
# End-to-end WRITE test for the pereprava NFS volume — no root required.
#
# Prerequisites: Nothing Phone connected & unlocked in "File transfer" mode,
# pereprava built (target/debug/pereprava), libnfs tools installed
# (`brew install libnfs`), ptpcamerad killer handled inside.
#
# Usage: scripts/e2e-write-test.sh [--size-bytes N]

set -e
cd "$(dirname "$0")/.."

PORT=34568
SIZE=${1:-204800}
DIR="/1/pereprava-e2e"
LOCAL=/tmp/pv-e2e-src.bin
BACK=/tmp/pv-e2e-back.bin

say(){ print -P "%F{cyan}== $* ==%f"; }

say "build check"
cargo build -q

say "ptpcamerad guard"
( for i in $(seq 1 600); do pkill -9 ptpcamerad 2>/dev/null; sleep 0.5; done ) &
KILLER=$!
sleep 1

cleanup_server(){ pkill -f "pereprava mount" 2>/dev/null || true; }
trap 'kill $KILLER 2>/dev/null; cleanup_server' EXIT

say "device present?"
target/debug/pereprava info >/dev/null || {
  print -P "%F{red}no MTP session — unlock phone / pick File transfer mode%f"; exit 1; }

say "prepare remote dir"
target/debug/pereprava mkdir "$DIR" >/dev/null 2>&1 || true

say "start serve-only NFS (writable)"
# NOTE: do NOT pass --export here — libnfs resolves the FULL url path
# against the device root; a custom export shifts levels and breaks MNT.
nohup target/debug/pereprava mount --serve-only --port $PORT \
      --allow-unprivileged-source-port \
      >/tmp/pv-e2e-server.log 2>&1 &
sleep 5

say "generate payload ($SIZE bytes)"
head -c $SIZE /dev/urandom > $LOCAL

U="nfs://127.0.0.1:$PORT/1/pereprava-e2e/wtest.bin?version=3&mountport=$PORT"

say "WRITE via nfs-cp"
timeout 90 nfs-cp $LOCAL "$U"

say "READ via nfs-cat + compare"
timeout 90 nfs-cat "$U" > $BACK
cmp $LOCAL $BACK && echo "BYTE-EXACT ✓"

say "cross-check on device via CLI (second session)"
cleanup_server; sleep 2
target/debug/pereprava ls "$DIR" | grep wtest.bin
rm -f /tmp/pv-e2e-src.bin $BACK

say "cleanup remote dir"
target/debug/pereprava rm "$DIR" -r

print -P "%F{green}E2E WRITE TEST PASSED%f"
