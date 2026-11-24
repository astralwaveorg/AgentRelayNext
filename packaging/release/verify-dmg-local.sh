#!/bin/bash
# Verify a locally built unsigned DMG against a published SHA-256. This script
# checks integrity and contents only; it does not claim Developer ID, notarization
# or Gatekeeper acceptance.
set -euo pipefail
[ "$#" -eq 2 ] || { echo "用法：verify-dmg-local.sh <dmg> <expected-sha256>" >&2; exit 2; }
DMG="$1"; EXPECTED="$2"
[[ "$EXPECTED" =~ ^[0-9a-f]{64}$ ]] || { echo "SHA-256 格式错误" >&2; exit 2; }
[ -f "$DMG" ] && [ ! -L "$DMG" ] || { echo "需要常规 DMG 文件" >&2; exit 1; }
ACTUAL="$(/usr/bin/shasum -a 256 "$DMG" | awk '{print $1}')"
[ "$ACTUAL" = "$EXPECTED" ] || { echo "SHA-256 不匹配" >&2; exit 1; }
MOUNT="$(mktemp -d /private/tmp/relay-dmg-verify.XXXXXX)"
cleanup() { /usr/bin/hdiutil detach "$MOUNT" >/dev/null 2>&1 || true; rmdir "$MOUNT"; }
trap cleanup EXIT
/usr/bin/hdiutil attach -readonly -nobrowse -noautoopen -mountpoint "$MOUNT" "$DMG" >/dev/null
[ -d "$MOUNT/RelayTaskWindow.app" ] && [ ! -L "$MOUNT/RelayTaskWindow.app" ] ||
  { echo "DMG 内缺少常规 App" >&2; exit 1; }
[ -x "$MOUNT/RelayTaskWindow.app/Contents/Resources/relay/relayctl" ] ||
  { echo "DMG 内缺少 relayctl" >&2; exit 1; }
"$MOUNT/RelayTaskWindow.app/Contents/Resources/relay/relayctl" version
echo "本地 DMG 摘要与内容检查通过；未做 Developer ID/公证/Gatekeeper 声明。"
