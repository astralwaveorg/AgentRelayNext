#!/bin/bash
# Verify a final DMG on the release operator's Mac. Never signs, notarizes or runs its content.
set -euo pipefail
[ "$#" -eq 3 ] || { echo "用法：verify-dmg.sh <signed.dmg> <expected-sha256> <expected-Team-ID>" >&2; exit 2; }
dmg="$1"; expected="$2"; team="$3"
fail() { echo "发行验证失败：$*" >&2; exit 1; }
[[ "$expected" =~ ^[0-9a-f]{64}$ && "$team" =~ ^[A-Z0-9]{10}$ ]] ||
  { echo "SHA-256 或 Team ID 格式错误" >&2; exit 2; }
[ -f "$dmg" ] && [ ! -L "$dmg" ] || fail "需要常规 DMG 文件"
dmg="$(cd "$(dirname "$dmg")" && pwd -P)/$(basename "$dmg")"
hash="$(/usr/bin/shasum -a 256 < "$dmg")"
[ "${hash%% *}" = "$expected" ] || fail "下载文件 SHA-256 不匹配"

/usr/bin/xcrun stapler validate "$dmg"
/usr/sbin/spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg"
req="anchor apple generic and certificate leaf[subject.OU] = \"$team\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
/usr/bin/codesign --verify --strict -R "$req" "$dmg"

work="$(mktemp -d /private/tmp/relay-dmg-verify.XXXXXX)"
mount="$work/mount"
mkdir "$mount"
attached=0
cleanup() {
  if [ "$attached" -eq 1 ] && ! /usr/bin/hdiutil detach "$mount" >/dev/null; then
    echo "请手动卸载 $mount；保留挂载目录" >&2
    return 1
  fi
  rmdir "$mount" "$work"
}
trap cleanup EXIT
/usr/bin/hdiutil attach -readonly -nobrowse -noautoopen -mountpoint "$mount" "$dmg" >/dev/null
attached=1
app="$mount/RelayTaskWindow.app"
helper="$app/Contents/Resources/relay/relayctl"
[ -d "$app" ] && [ ! -L "$app" ] && [ -f "$helper" ] && [ ! -L "$helper" ] || fail "缺少正式应用或后台执行体"
/usr/bin/codesign --verify --deep --strict -R "$req" "$app"
/usr/bin/codesign --verify --strict -R "$req" "$helper"
/usr/sbin/spctl --assess --type execute --verbose=4 "$app"
for signed in "$app" "$helper"; do
  signature="$(/usr/bin/codesign --display --verbose=4 "$signed" 2>&1)"
  grep -Eq '^CodeDirectory .*flags=.*\(.*runtime' <<< "$signature" || fail "缺少 Hardened Runtime"
  grep -q '^Timestamp=' <<< "$signature" || fail "缺少 Apple 可信时间戳"
done
printf '发行包通过 SHA-256、公证票据、Developer ID (%s)、Hardened Runtime、时间戳与 Gatekeeper 检查。\n' "$team"
