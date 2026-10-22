#!/bin/sh
# Builds Backroom.app (macOS). Without an .app bundle with a microphone entry
# in Info.plist the system will not let the program record.
#
#   scripts/macos-app.sh            # architecture of this Mac
#   scripts/macos-app.sh universal  # Intel + Apple Silicon in one binary
set -e
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

if [ "$1" = "universal" ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release -p backroom --target aarch64-apple-darwin
  cargo build --release -p backroom --target x86_64-apple-darwin
  BIN=target/backroom-universal
  lipo -create -output "$BIN" \
    target/aarch64-apple-darwin/release/backroom \
    target/x86_64-apple-darwin/release/backroom
else
  cargo build --release -p backroom
  BIN=target/release/backroom
fi

APP=target/Backroom.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/Backroom"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Backroom</string>
  <key>CFBundleDisplayName</key><string>Backroom</string>
  <key>CFBundleExecutable</key><string>Backroom</string>
  <key>CFBundleIdentifier</key><string>pl.backroom.client</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>Backroom needs the microphone so your friends can hear you.</string>
</dict>
</plist>
PLIST
# Ad-hoc signature: enough for macOS to remember permissions (microphone, accessibility).
codesign --force --deep --sign - "$APP"
echo "Done: $APP"
