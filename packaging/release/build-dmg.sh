#!/bin/bash
# Build a local, unsigned DMG for GitHub/self-hosting. This does NOT produce a
# Developer ID signed or notarized artifact, and it never claims Gatekeeper
# acceptance. Users must explicitly approve the first open in System Settings.
set -euo pipefail
[ "$#" -eq 2 ] || { echo "用法：build-dmg.sh <RelayTaskWindow.app> <output.dmg>" >&2; exit 2; }
APP="$1"; OUT="$2"
[ -d "$APP" ] && [ ! -L "$APP" ] || { echo "需要常规 .app：$APP" >&2; exit 1; }
[[ "$OUT" = /* ]] || { echo "输出 DMG 必须是绝对路径" >&2; exit 2; }
case "$OUT" in *.dmg) ;; *) echo "输出必须以 .dmg 结尾" >&2; exit 2;; esac
RELAY="$APP/Contents/Resources/relay/relayctl"
[ -x "$RELAY" ] || { echo "缺少 relayctl" >&2; exit 1; }
if [ "${RELAY_NO_EXEC:-0}" = 1 ]; then
  VERSION="$(cat "$APP/Contents/Resources/relay/version.json")"
else
  VERSION="$("$RELAY" version)"
fi
WORK="$(mktemp -d /private/tmp/relay-dmg-build.XXXXXX)"
MOUNT="$(mktemp -d /private/tmp/relay-dmg-mount.XXXXXX)"
STAGE="$WORK/stage"
cleanup() {
  /usr/bin/hdiutil detach "$MOUNT" >/dev/null 2>&1 || true
  rm -rf "$WORK" "$MOUNT"
}
trap cleanup EXIT
mkdir "$STAGE"
/usr/bin/ditto "$APP" "$STAGE/RelayTaskWindow.app"
ln -s /Applications "$STAGE/Applications"
cat > "$STAGE/README.txt" <<'TXT'
Agent Relay — local preview

This DMG is unsigned and not notarized. macOS may block the first launch.
After dragging RelayTaskWindow.app to Applications, Control-click the app and
choose Open, or allow it in System Settings → Privacy & Security. Do not disable
Gatekeeper globally.

Verify the downloaded DMG SHA-256 against the sidecar file next to it (or the
release SHA256SUMS) before opening. The app runs locally; it does not install a
system daemon or bundle vendor CLIs.

On first launch the app offers to enable the local preview. Enabling creates
~/.agent-relay-next and registers a LaunchAgent for the current user only; it
does not modify system settings and it can be disabled again from the app.
TXT
rm -f "$OUT"
/usr/bin/hdiutil create -volname "Agent Relay" -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null
SHA="$(/usr/bin/shasum -a 256 "$OUT" | awk '{print $1}')"
/usr/bin/hdiutil attach -readonly -nobrowse -noautoopen -mountpoint "$MOUNT" "$OUT" >/dev/null
[ -d "$MOUNT/RelayTaskWindow.app" ] || { echo "DMG 内缺少 App" >&2; exit 1; }
[ -L "$MOUNT/Applications" ] || { echo "DMG 内缺少 Applications 链接" >&2; exit 1; }
[ -x "$MOUNT/RelayTaskWindow.app/Contents/Resources/relay/relayctl" ] ||
  { echo "DMG 内缺少 relayctl" >&2; exit 1; }
if [ "${RELAY_NO_EXEC:-0}" = 1 ]; then
  MOUNT_VERSION="$(cat "$MOUNT/RelayTaskWindow.app/Contents/Resources/relay/version.json")"
else
  MOUNT_VERSION="$("$MOUNT/RelayTaskWindow.app/Contents/Resources/relay/relayctl" version)"
fi
/usr/bin/hdiutil detach "$MOUNT" >/dev/null
printf '%s\n' "$SHA" > "$OUT.sha256"
cat > "$OUT.manifest.json" <<JSON
{"product":"$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")",
 "build":"$(/usr/libexec/PlistBuddy -c 'Print CFBundleVersion' "$APP/Contents/Info.plist")",
 "artifact":"$(basename "$OUT")","sha256":"$SHA",
 "versions":$VERSION,"signed":false,"notarized":false,
 "gatekeeper":"first launch requires explicit user approval",
 "platform":"macOS arm64 (deployment target 14.0)"}
JSON
printf 'DMG: %s\nSHA-256: %s\n' "$OUT" "$SHA"
printf 'DMG 内版本：%s\n' "$MOUNT_VERSION"
