#!/bin/zsh
# Install pereprava as a system daemon that auto-mounts any connected
# Android phone into Finder — zero prompts after installation.
#
# What it does:
#   1. cargo build --release
#   2. writes /Library/LaunchDaemons/com.egorich.pereprava.plist (root)
#   3. bootstraps it with launchctl
#
# Requires: ONE sudo authorization during install.
# Remove:   sudo launchctl bootout system/com.egorich.pereprava \
#             && sudo rm /Library/LaunchDaemons/com.egorich.pereprava.plist

set -e
cd "$(dirname "$0")/.."

echo "== building release binary =="
cargo build --release -q
BIN="$PWD/target/release/pereprava"

PLIST=/Library/LaunchDaemons/com.egorich.pereprava.plist
LOG=/var/log/pereprava.log

read -r -d '' XML <<EOF2 || true
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.egorich.pereprava</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN</string>
        <string>watch</string>
        <string>--port</string><string>34567</string>
        <string>--path</string><string>/Volumes/pereprava</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$LOG</string>
    <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF2

TMP=$(mktemp)
printf '%s\n' "$XML" > "$TMP"

echo "== installing (needs your password once) =="
sudo cp "$TMP" "$PLIST"
sudo chown root:wheel "$PLIST"
sudo launchctl bootstrap system "$PLIST" 2>/dev/null || \
  sudo launchctl load -w "$PLIST"
rm -f "$TMP"

echo "done. Logs: $LOG"
echo "Phone will auto-appear in Finder on every connection."
