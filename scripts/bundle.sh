#!/bin/sh
# Construit « YouTube Live Translator.app » (double-cliquable) à partir du binaire release.
set -e
cd "$(dirname "$0")/.."
cargo build --release
APP="target/YouTube Live Translator.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/ytlt "$APP/Contents/MacOS/ytlt"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>YouTube Live Translator</string>
  <key>CFBundleDisplayName</key><string>YouTube Live Translator</string>
  <key>CFBundleIdentifier</key><string>com.younss.ytlt</string>
  <key>CFBundleExecutable</key><string>ytlt</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppTransportSecurity</key><dict><key>NSAllowsLocalNetworking</key><true/></dict>
</dict></plist>
PLIST
codesign --force --deep -s - "$APP" 2>/dev/null || true
echo "OK : $APP"
