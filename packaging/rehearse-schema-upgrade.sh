#!/bin/bash
# 验证真实 v8 数据和 daemon；旧包仅借用新 UI、配置与离线提交助手。
set -euo pipefail
umask 077
[ "$#" -eq 2 ] || { echo "用法：rehearse-schema-upgrade.sh <v8 relayctl> <v9 ZIP>" >&2; exit 2; }
SCRIPT="$(cd "$(dirname "$0")" && pwd -P)"
OLD="$1"; NEXT="$2"
QA="$(mktemp -d /private/tmp/r8.XXXXXX)"
trap 'rm -rf "$QA"' EXIT
"$OLD" version > "$QA/v8.json"
[ "$(/usr/bin/plutil -extract db_schema raw -o - "$QA/v8.json")" = 8 ]
VERSION="$(/usr/bin/plutil -extract product raw -o - "$QA/v8.json")"
ditto -x -k "$NEXT" "$QA/old"
APP="$QA/old/RelayTaskWindow.app"
R="$APP/Contents/Resources/relay"
mv "$R/relayctl" "$R/demo-helper"
cp "$OLD" "$R/relayctl-v8"
cat > "$R/relayctl" <<'EOF'
#!/bin/bash
root="$(dirname "$0")"
case "${1:-}" in local-preview) exec "$root/demo-helper" "$@";; esac
exec "$root/relayctl-v8" "$@"
EOF
chmod 700 "$R/relayctl"
(cd "$R" && shasum -a 256 relayctl relayctl-v8 demo-helper > SHA256)
plutil -replace CFBundleShortVersionString -string "$VERSION" "$APP/Contents/Info.plist"
(cd "$QA/old" && ditto -c -k --keepParent RelayTaskWindow.app "$QA/old.zip")
shasum -a 256 "$QA/old.zip" > "$QA/old.zip.sha256"
"$SCRIPT/rehearse-upgrade.sh" "$QA/old.zip" "$NEXT"
