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

# Plist values are XML text: escape the five predefined entities so paths
# containing '&', '<', '>' or '"' cannot produce a corrupt/injected plist.
xml_escape() {
  print -r -- "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' \
                         -e 's/"/\&quot;/g' -e "s/'/\&apos;/g"
}

echo "== building release binary =="
cargo build --release -q
SRC_BIN="$ROOT/target/release/pereprava"

# The LaunchDaemon runs as root, so it must NOT execute a binary from the
# user's writable checkout: anything running as that user could replace the
# file and obtain root execution on the next restart. Install a root-owned
# copy under /usr/local/libexec and point the plist at it.
PREFIX=/usr/local/libexec
BIN="$PREFIX/pereprava"
BIN_XML="$(xml_escape "$BIN")"

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
        <string>$BIN_XML</string>
        <string>watch</string>
        <string>--port</string><string>34567</string>
        <string>--path</string><string>/Volumes/pereprava</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>StandardOutPath</key><string>$(xml_escape "$LOG")</string>
    <key>StandardErrorPath</key><string>$(xml_escape "$LOG")</string>
</dict>
</plist>
EOF2

TMP=$(mktemp)
printf '%s\n' "$XML" > "$TMP"

echo "== installing daemon (needs your password once) =="
sudo mkdir -p "$PREFIX"
sudo cp "$SRC_BIN" "$BIN"
sudo chown root:wheel "$BIN"
sudo chmod 0755 "$BIN"
sudo cp "$TMP" "$PLIST"
sudo chown root:wheel "$PLIST"
sudo chmod 0644 "$PLIST"
# Re-run safely: bootout first, then bootstrap the fresh definition.
sudo launchctl bootout system/com.egorich.pereprava 2>/dev/null || true
sudo launchctl bootstrap system "$PLIST" 2>/dev/null || \
  sudo launchctl load -w "$PLIST"
rm -f "$TMP"

echo "== building Tauri widget (Rust + Svelte) =="
# A clean checkout has no ui/node_modules; vite is not on PATH otherwise.
( cd "$ROOT/crates/widget/ui" && npm ci )
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
        <string>$(xml_escape "$APPS/Pereprava.app/Contents/MacOS/pereprava-widget")</string>
    </array>
    <key>RunAtLoad</key><true/>
    <!-- Restart on crash only: a clean exit (the tray "Выход") must stick. -->
    <key>KeepAlive</key>
    <dict><key>SuccessfulExit</key><false/></dict>
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
echo
echo "NOTE (macOS 26 Tahoe): if the menu-bar icon does not appear, enable"
echo "  System Settings -> Menu Bar -> Allow in the Menu Bar -> Pereprava."
