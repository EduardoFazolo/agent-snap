#!/bin/sh
# Wraps a binary in dist/AgentSnap.app (menu bar only) and signs it.
# Usage: scripts/make-app.sh [path/to/binary]   (default: target/release/agent-snap)
# Signs with "AgentSnap Dev" when that identity exists (so TCC permissions survive rebuilds), else ad-hoc.
set -e
cd "$(dirname "$0")/.."
BIN="${1:-target/release/agent-snap}"
if [ ! -x "$BIN" ]; then
  echo "binary not found: $BIN (build first, e.g. cargo build --release)" >&2
  exit 1
fi
APP=dist/AgentSnap.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/AgentSnap"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.fazolo.agent-snap</string>
  <key>CFBundleName</key><string>AgentSnap</string>
  <key>CFBundleDisplayName</key><string>Agent Snap</string>
  <key>CFBundleExecutable</key><string>AgentSnap</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppleEventsUsageDescription</key><string>Reads the active browser tab URL to label recordings.</string>
  <key>NSScreenCaptureUsageDescription</key><string>Captures your screen while recording.</string>
</dict></plist>
PLIST
# Prefer a stable identity so macOS permissions survive rebuilds (see README).
IDENTITY="${CODESIGN_IDENTITY:-$(security find-identity -v -p codesigning | grep -o '"AgentSnap Dev"' | head -1 | tr -d '"')}"
codesign --force --sign "${IDENTITY:--}" "$APP" >/dev/null
echo "signed with: ${IDENTITY:-ad-hoc}"
echo "$APP"
