#!/bin/sh
# Builds release binary and wraps it in dist/AgentSnap.app (ad-hoc signed, menu bar only).
set -e
cd "$(dirname "$0")/.."
swift build -c release
APP=dist/AgentSnap.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp .build/release/agent-snap "$APP/Contents/MacOS/AgentSnap"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.fazolo.agent-snap</string>
  <key>CFBundleName</key><string>AgentSnap</string>
  <key>CFBundleDisplayName</key><string>Agent Snap</string>
  <key>CFBundleExecutable</key><string>AgentSnap</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppleEventsUsageDescription</key><string>Reads the active browser tab URL to label recordings.</string>
  <key>NSScreenCaptureUsageDescription</key><string>Captures keyframes of your screen while recording.</string>
</dict></plist>
PLIST
# Prefer a stable identity so macOS permissions survive rebuilds (see README).
IDENTITY="${CODESIGN_IDENTITY:-$(security find-identity -v -p codesigning | grep -o '"AgentSnap Dev"' | head -1 | tr -d '"')}"
codesign --force --sign "${IDENTITY:--}" "$APP" >/dev/null
echo "signed with: ${IDENTITY:-ad-hoc}"
echo "$APP"
