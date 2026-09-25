#!/bin/zsh
# Control the pereprava LaunchDaemon.
#
# The daemon runs in the system domain (root), so every action needs sudo.
# While it is running it owns the single MTP session — stop it before using
# the CLI or the NFS e2e harness directly.
#
# Usage: scripts/daemon.sh {stop|start|restart|status|log}

LABEL=com.egorich.pereprava
PLIST=/Library/LaunchDaemons/$LABEL.plist
LOG=/var/log/pereprava.log
# The daemon runs the root-owned copy, not the user-writable checkout.
PREFIX=/usr/local/libexec
BIN="$PREFIX/pereprava"
SRC_BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/pereprava"

case "${1:-status}" in
  stop)
    sudo launchctl bootout system/$LABEL && echo "daemon stopped"
    ;;
  start)
    sudo launchctl bootstrap system "$PLIST" && echo "daemon started"
    ;;
  restart)
    sudo launchctl kickstart -k system/$LABEL && echo "daemon restarted"
    ;;
  update)
    # Rebuild, install the root-owned binary, repoint the plist at it, and
    # reload the service (bootout+bootstrap, because launchd caches the job
    # definition and a bare kickstart keeps using the old program path).
    ( cd "$(dirname "$0")/.." && cargo build --release -q )
    sudo mkdir -p "$PREFIX"
    sudo cp "$SRC_BIN" "$BIN"
    sudo chown root:wheel "$BIN"
    sudo chmod 0755 "$BIN"
    # The daemon must never run from the user-writable checkout.
    sudo /usr/libexec/PlistBuddy -c "Set :ProgramArguments:0 $BIN" "$PLIST" 2>/dev/null \
      || sudo /usr/libexec/PlistBuddy -c "Add :ProgramArguments:0 string $BIN" "$PLIST"
    sudo launchctl bootout system/$LABEL 2>/dev/null || true
    sudo launchctl bootstrap system "$PLIST" 2>/dev/null \
      || sudo launchctl load -w "$PLIST"
    echo "daemon updated + restarted (now running $BIN)"
    ;;
  status)
    if sudo launchctl print system/$LABEL 2>/dev/null | sed -n '1,14p'; then
      :
    else
      echo "daemon not loaded"
    fi
    ;;
  log)
    tail -n "${2:-40}" "$LOG"
    ;;
  *)
    echo "usage: $0 {stop|start|restart|update|status|log [n]}" >&2
    exit 2
    ;;
esac
