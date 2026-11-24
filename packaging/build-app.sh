#!/bin/bash
# 编译最小 macOS 任务窗口并组装 .app（不签名、不公证）。
# 用法：build-app.sh <RelayTaskWindow.app> <version> <build>
set -euo pipefail
APP="$1"; VERSION="$2"; BUILD="$3"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
swiftc -O -swift-version 5 -parse-as-library -target "${RELAY_MACOS_TARGET:-arm64-apple-macosx14.0}" \
  -o "$APP/Contents/MacOS/RelayTaskWindow" "$ROOT/ui/RelayTaskWindow/RelayTaskWindow.swift"
icon_tmp="$(mktemp -d /private/tmp/relay-icon.XXXXXX)"
trap 'rm -rf "$icon_tmp"' EXIT
mkdir "$icon_tmp/RelayIcon.iconset"
swift "$ROOT/ui/RelayTaskWindow/DrawIcon.swift" "$icon_tmp/RelayIcon.iconset"
/usr/bin/iconutil -c icns "$icon_tmp/RelayIcon.iconset" -o "$APP/Contents/Resources/RelayIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleDevelopmentRegion</key><string>zh_CN</string>
  <key>CFBundleExecutable</key><string>RelayTaskWindow</string>
  <key>CFBundleIdentifier</key><string>net.relay.candidate.taskwindow</string>
  <key>CFBundleIconFile</key><string>RelayIcon</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Relay 任务</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
echo "built $APP ($VERSION / $BUILD)"
