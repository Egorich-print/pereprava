#!/bin/zsh
# Install pereprava as a system daemon that auto-mounts any connected
# Android phone into Finder — zero prompts after installation.
#
# What it does:
#   1. builds the CLI release binary
#   2. writes /Library/LaunchDaemons/com.egorich.pereprava.plist (root)
#   3. builds + installs the Tauri/Svelte status widget (~/Applications)
#   4. registers both with launchctl
#
# Requires: ONE sudo authorization during install.
# Remove:   sudo launchctl bootout system/com.egorich.pereprava \
#             && sudo rm /Library/LaunchDaemons/com.egorich.pereprava.plist \
#             && launchctl bootout gui/$UID/com.egorich.pereprava.widget \
#             && rm -f ~/Library/LaunchAgents/com.egorich.pereprava.widget.plist

set -e
cd "$(dirname "$0")/.."
ROOT="$PWD"

echo "== building release binary =="
cargo build --release -q
BIN="$ROOT/target/release/pereprava"

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
    <key>ThrottleInterval</key><integer>10</integer>
    <key>StandardOutPath</key><string>$LOG</string>
    <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF2

TMP=$(mktemp)
printf '%s\n' "$XML" > "$TMP"

echo "== installing daemon (needs your password once) =="
sudo cp "$TMP" "$PLIST"
sudo chown root:wheel "$PLIST"
# Re-run safely: bootout first, then bootstrap the fresh definition.
sudo launchctl bootout system/com.egorich.pereprava 2>/dev/null || true
sudo launchctl bootstrap system "$PLIST" 2>/dev/null || \
  sudo launchctl load -w "$PLIST"
rm -f "$TMP"

echo "== building Tauri widget (Rust + Svelte) =="
( cd "$ROOT/crates/widget" && cargo tauri build )

echo "== installing widget (user agent, no sudo) =="
APPS="$HOME/Applications"
mkdir -p "$APPS"
rm -rf "$APPS/Pereprava.app"
cp -R "$ROOT/crates/widget/target/release/bundle/macos/Pereprava.app" "$APPS/Pereprava.app"

AGENTS_DIR="$HOME/Library/LaunchAgents"
mkdir -p "$AGENTS_DIR"
WIDGET_PLIST="$AGENTS_DIR/com.egorich.pereprava.widget.plist"
cat > "$WIDGET_PLIST" <<XML2
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.egorich.pereprava.widget</string>
    <key>ProgramArguments</key>
    <array>
        <string>$APPS/Pereprava.app/Contents/MacOS/pereprava-widget</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>LimitLoadToSessionType</key><string>Aqua</string>
</dict>
</plist>
XML2

# Retire the legacy AppKit menu-bar agent if it is still installed.
launchctl bootout gui/$UID/com.egorich.pereprava.menubar 2>/dev/null || true
rm -f "$AGENTS_DIR/com.egorich.pereprava.menubar.plist"

launchctl bootout gui/$UID/com.egorich.pereprava.widget 2>/dev/null || true
launchctl bootstrap gui/$UID "$WIDGET_PLIST" && echo "виджет запущен 🌉"

echo "done. Daemon log: $LOG"
echo "Phone will auto-appear in Finder on every connection."
