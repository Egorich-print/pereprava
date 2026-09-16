#!/bin/zsh
# End-to-end OVERWRITE test — targets the write-back bugs fixed in v0.6.
#
# Checks that repeatedly writing the same path through the NFS volume:
#   * leaves exactly ONE object on the device (no orphaned duplicates from a
#     second COMMIT), and
#   * ends with the LAST content (no silent truncation to an empty stage).
#
# No root required. Prerequisites: phone unlocked in "File transfer" mode,
# `brew install libnfs`.
#
# Usage: scripts/e2e-overwrite-test.sh

set -e
cd "$(dirname "$0")/.."

PORT=34569
DIR="/1/pereprava-ovw"
A=/tmp/pv-ovw-a.bin
B=/tmp/pv-ovw-b.bin

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

say "serve-only NFS (writable)"
nohup target/debug/pereprava mount --serve-only --port $PORT \
      --allow-unprivileged-source-port >/tmp/pv-ovw-server.log 2>&1 &
sleep 5

U="nfs://127.0.0.1:$PORT${DIR}/ovw.bin?version=3&mountport=$PORT"

say "first write (content A)"
head -c 131072 /dev/urandom > $A
timeout 90 nfs-cp $A "$U"

say "second write, same path (content B)"
head -c 200000 /dev/urandom > $B
timeout 90 nfs-cp $B "$U"

say "third write, same path (content A again, different size)"
timeout 90 nfs-cp $A "$U"

say "read back via NFS"
timeout 90 nfs-cat "$U" > /tmp/pv-ovw-back.bin
cmp $A /tmp/pv-ovw-back.bin && echo "content matches LAST write ✓"

say "cross-check on device (exactly one object?)"
cleanup_server; sleep 2
COUNT=$(target/debug/pereprava ls "$DIR" | grep -c "ovw.bin" || true)
print -P "objects named ovw.bin on device: %F{yellow}$COUNT%f"
if [ "$COUNT" -ne 1 ]; then
  print -P "%F{red}FAIL: expected exactly 1 object, found $COUNT%f"
  target/debug/pereprava ls "$DIR"
  exit 1
fi

say "cleanup"
target/debug/pereprava rm "$DIR" -r
rm -f $A $B /tmp/pv-ovw-back.bin

print -P "%F{green}E2E OVERWRITE TEST PASSED%f"
