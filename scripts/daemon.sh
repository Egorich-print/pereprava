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
    echo "usage: $0 {stop|start|restart|status|log [n]}" >&2
    exit 2
    ;;
esac
