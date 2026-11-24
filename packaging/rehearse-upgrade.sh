#!/bin/bash
# 两个实际版本、独立 launchd 标签和 HOME；不接触本机安装。
set -euo pipefail
umask 077
[ "$#" -eq 2 ] || { echo "用法：rehearse-upgrade.sh <旧 ZIP> <新 ZIP>" >&2; exit 2; }
SCRIPT="$(cd "$(dirname "$0")" && pwd -P)"
OLD="$1"; NEXT="$2"
WORK="$(mktemp -d /private/tmp/ru.XXXXXX)"
export HOME="$WORK/home"
export RELAY_PREVIEW_LABEL="net.relay.candidate.local-preview.qa.${WORK##*.}"
SERVICE="gui/$(id -u)/$RELAY_PREVIEW_LABEL"
cleanup() {
  local code=$?
  launchctl bootout "$SERVICE" >/dev/null 2>&1 || true
  if [ "$code" -eq 0 ] && [ -z "${RELAY_KEEP:-}" ]; then
    rm -rf "$WORK"
  else
    echo "验收现场：$WORK" >&2
  fi
}
trap cleanup EXIT
mkdir "$HOME" "$WORK/broken"
"$SCRIPT/install-local-preview.sh" "$OLD"
APP="$HOME/Applications/RelayTaskWindow.app"
BIN="$APP/Contents/Resources/relay/relayctl"
ROOT="$HOME/.agent-relay-next"
INST="$ROOT/instance"
SOCK="$INST/relay.sock"
submit="$("$BIN" local-preview submit)"
printf '%s' "$submit" > "$WORK/submit.json"
task="$(/usr/bin/plutil -extract task.id raw -o - "$WORK/submit.json")"
for _ in $(seq 1 100); do
  "$BIN" status --socket "$SOCK" --task-id "$task" > "$WORK/task.json"
  [ "$(/usr/bin/plutil -extract task.state raw -o - "$WORK/task.json")" = awaiting_review ] && break
  sleep 0.1
done
[ "$(/usr/bin/plutil -extract task.state raw -o - "$WORK/task.json")" = awaiting_review ]
artifact() { "$BIN" artifact --socket "$SOCK" --task-id "$task" --attempt-no 1 --kind diff; }
artifact > "$WORK/old-artifact.json"
before="$(/usr/bin/sqlite3 -readonly "$INST/relay.sqlite3" 'SELECT count(*) FROM events')"
old_version="$(/usr/bin/plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")"
schema() { /usr/bin/sqlite3 -readonly "$INST/relay.sqlite3" "SELECT value FROM meta WHERE key='schema_version'"; }
old_schema="$(schema)"
old_inode="$(stat -f %i "$INST/relay.sqlite3")"

# 候选静态检查全部通过，有意只令 daemon 启动失败。
ditto -x -k "$NEXT" "$WORK/broken"
R="$WORK/broken/RelayTaskWindow.app/Contents/Resources/relay"
mv "$R/relayctl" "$R/relayctl-real"
cat > "$R/relayctl" <<'SH'
#!/bin/bash
if [ "${1:-}" = daemon ]; then exit 59; fi
exec "$(dirname "$0")/relayctl-real" "$@"
SH
chmod +x "$R/relayctl"
(cd "$R" && shasum -a 256 relayctl > SHA256)
(cd "$WORK/broken" && ditto -c -k --keepParent RelayTaskWindow.app "$WORK/broken.zip")
shasum -a 256 "$WORK/broken.zip" > "$WORK/broken.zip.sha256"
if "$SCRIPT/upgrade-local-preview.sh" "$WORK/broken.zip" > "$WORK/rollback.log" 2>&1; then
  echo "本应启动失败并回滚" >&2; exit 1
fi
rg -q '旧版已恢复并通过启动检查' "$WORK/rollback.log"
[ "$(/usr/bin/plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")" = "$old_version" ]
[ "$(schema)" = "$old_schema" ]
[ "$(stat -f %i "$INST/relay.sqlite3")" = "$old_inode" ]
artifact > "$WORK/restored-artifact.json"
cmp "$WORK/old-artifact.json" "$WORK/restored-artifact.json"

"$SCRIPT/upgrade-local-preview.sh" "$NEXT"
[ "$(/usr/bin/plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")" != "$old_version" ]
[ "$(schema)" = 9 ]
if [ "$old_schema" = 8 ]; then
  [ "$(stat -f %i "$INST/relay.sqlite3")" != "$old_inode" ]
  [ "$(find "$HOME/Applications" -path '*/database-exchange/relay.sqlite3' -inum "$old_inode" | wc -l | tr -d ' ')" = 1 ]
fi
artifact > "$WORK/new-artifact.json"
cmp "$WORK/old-artifact.json" "$WORK/new-artifact.json"
[ "$before" = "$(/usr/bin/sqlite3 -readonly "$INST/relay.sqlite3" 'SELECT count(*) FROM events')" ]
"$APP/Contents/MacOS/RelayTaskWindow" --socket "$SOCK" --dump > "$WORK/ui.txt"
rg -q 'wire 1，DB schema 9' "$WORK/ui.txt"
rg -q '待验收' "$WORK/ui.txt"
test ! -e "$INST/upgrade.maintenance"
test ! -e "$ROOT/upgrade.lock"
# 新版维护标记应拒绝新提交，但保留对既有制品的读取。
: > "$INST/upgrade.maintenance"
if "$BIN" local-preview submit > "$WORK/maintenance.out" 2>&1; then
  echo "维护期间竟接收任务" >&2; exit 1
fi
rg -q 'SERVICE_UNAVAILABLE' "$WORK/maintenance.out"
artifact > "$WORK/frozen-artifact.json"
cmp "$WORK/old-artifact.json" "$WORK/frozen-artifact.json"
rm "$INST/upgrade.maintenance"
echo "跨版本升级与失败回滚通过：schema $old_schema → $(schema)，原库、待验收任务、事件数与补丁已核对。"
