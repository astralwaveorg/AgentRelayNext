#!/bin/bash
# 手动回退：从统一的 .relay-backup.* 恢复 App 与数据快照。
# 用法：rollback.sh <dest-root> [backup-dir | .relay-backup 后缀]
#
# 备份统一布局（install/upgrade/rollback 共用）：
#   <dest>/.relay-backup.XXXXXX/
#     RelayTaskWindow.app
#     data/relay.sqlite3          (maintenance snapshot)
#     metadata.json
#
# 回退前一定会为当前状态再建一个同格式备份；失败时保留现场并尝试恢复。
set -euo pipefail
umask 077
[ "$#" -ge 1 ] || { echo "用法：rollback.sh <dest-root> [backup]" >&2; exit 2; }
DEST="$1"
APP="$DEST/RelayTaskWindow.app"
ROOT="$HOME/.agent-relay-next"
INST="$ROOT/instance"
LABEL="${RELAY_PREVIEW_LABEL:-net.relay.candidate.local-preview}"
case "$LABEL" in net.relay.candidate.local-preview*) ;; *) exit 2;; esac
case "$LABEL" in *[!a-zA-Z0-9.-]*) exit 2;; esac
DOMAIN="gui/$(id -u)"
SERVICE="$DOMAIN/$LABEL"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
BIN="Contents/Resources/relay/relayctl"
fail() { echo "$*" >&2; exit 1; }
read_plist() { /usr/bin/plutil -extract "$1" raw -o - "$2"; }

resolve_backup() {
  local arg="${1:-}" latest
  if [ -z "$arg" ]; then
    latest="$(ls -1dt "$DEST"/.relay-backup.*/ 2>/dev/null | head -1 || true)"
    [ -n "$latest" ] || fail "没有可回退的备份"
    printf '%s' "${latest%/}"
  else
    case "$arg" in
      /*) printf '%s' "$arg" ;;
      *) printf '%s' "$DEST/.relay-backup.$arg" ;;
    esac
  fi
}
BACKUP="$(resolve_backup "${2:-}")"
[ -d "$BACKUP" ] && [ ! -L "$BACKUP" ] || fail "备份目录无效：$BACKUP"
META="$BACKUP/metadata.json"
[ -f "$META" ] && [ ! -L "$META" ] || fail "备份缺少 metadata.json：$BACKUP"
meta() {
  /usr/bin/python3 -c '
import json,sys
value=json.load(open(sys.argv[1])).get(sys.argv[2])
print("" if value is None else value)' "$META" "$1"
}
TARGET_PRODUCT="$(meta product)"
TARGET_SCHEMA="$(meta db_schema)"
TARGET_SHA="$(meta app_sha256)"
SNAPSHOT_REL="$(meta database_snapshot)"
SNAPSHOT_SHA="$(meta database_snapshot_sha256)"
case "$TARGET_SCHEMA" in ''|*[!0-9]*) fail "metadata 缺少有效 db_schema";; esac
[ -n "$TARGET_PRODUCT" ] && [ -n "$TARGET_SHA" ] || fail "metadata 不完整"
[ -d "$BACKUP/RelayTaskWindow.app" ] && [ ! -L "$BACKUP/RelayTaskWindow.app" ] ||
  fail "备份缺少常规 App"
digest_dir() {
  (cd "$1" && find . -type f | LC_ALL=C sort | while IFS= read -r f; do
    /usr/bin/shasum -a 256 "$f"
  done | /usr/bin/shasum -a 256 | awk '{print $1}')
}
APP_ACTUAL="$(/usr/bin/shasum -a 256 "$BACKUP/RelayTaskWindow.app/$BIN" | awk '{print $1}')"
[ "$APP_ACTUAL" = "$TARGET_SHA" ] || fail "备份 App 摘要与 metadata 不一致"
SNAP_DIR="$BACKUP/${SNAPSHOT_REL:-data}"
[ -f "$SNAP_DIR/relay.sqlite3" ] && [ ! -L "$SNAP_DIR/relay.sqlite3" ] ||
  fail "备份缺少数据快照"
[ "$(digest_dir "$SNAP_DIR")" = "$SNAPSHOT_SHA" ] || fail "备份数据快照摘要不一致"

for path in "$DEST" "$ROOT" "$INST" "$PLIST"; do
  [ ! -L "$path" ] || fail "拒绝符号链接：$path"
done
[ "$(cd "$ROOT" && pwd -P)" = "$ROOT" ] && [ "$(cd "$DEST" && pwd -P)" = "$DEST" ] ||
  fail "试用路径不是物理路径"
[ -f "$PLIST" ] || fail "缺少 LaunchAgent：$PLIST"
[ "$(read_plist Label "$PLIST")" = "$LABEL" ] || fail "launchd 注册 Label 不匹配"

wait_stopped() {
  for _ in $(seq 1 100); do
    if ! launchctl print "$SERVICE" >/dev/null 2>&1 && [ ! -S "$INST/relay.sock" ]; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}
stop_service() {
  launchctl bootout "$SERVICE" >/dev/null 2>&1 || true
  wait_stopped
}
health() {
  for _ in $(seq 1 40); do
    if [ -S "$INST/relay.sock" ] &&
      "$1/$BIN" mcp-verify --socket "$INST/relay.sock" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  return 1
}
write_wrapper_plist() {
  local temp
  temp="$(mktemp "$HOME/Library/LaunchAgents/.relay-rollback.XXXXXX")"
  /usr/bin/plutil -create xml1 "$temp"
  /usr/bin/plutil -insert Label -string "$LABEL" "$temp"
  /usr/bin/plutil -insert ProgramArguments -json '[]' "$temp"
  local i=0 arg
  for arg in "$ROOT/run-daemon.sh" "$ROOT" "$INST" "$ROOT/config.json" "$APP/$BIN"; do
    /usr/bin/plutil -insert "ProgramArguments.$i" -string "$arg" "$temp"
    i=$((i + 1))
  done
  /usr/bin/plutil -insert RunAtLoad -bool true "$temp"
  /usr/bin/plutil -insert KeepAlive -bool true "$temp"
  /usr/bin/plutil -lint "$temp" >/dev/null
  chmod 600 "$temp"
  mv -f "$temp" "$PLIST"
}
current_schema() {
  /usr/bin/sqlite3 "$INST/relay.sqlite3" "select value from meta where key='schema_version';" \
    2>/dev/null || true
}

CURRENT_VERSION="$(read_plist CFBundleShortVersionString "$APP/Contents/Info.plist" 2>/dev/null || true)"
CURRENT_SCHEMA="$(current_schema)"
[ -n "$CURRENT_SCHEMA" ] || fail "无法读取当前数据 schema"
NEW_BACKUP=""
stopped=0
fail_recovery() {
  local code=$? restored=1
  trap - EXIT INT TERM
  set +e
  if [ "$code" -ne 0 ]; then
    echo "回退失败；保留现场：备份=$BACKUP 当前状态备份=${NEW_BACKUP:-未建立}" >&2
    if [ "$stopped" -eq 1 ] && [ -n "$NEW_BACKUP" ]; then
      stop_service || restored=0
      if [ "$restored" -eq 1 ] && [ -d "$NEW_BACKUP/RelayTaskWindow.app" ]; then
        rm -rf "$APP"
        mv "$NEW_BACKUP/RelayTaskWindow.app" "$APP" || restored=0
      fi
      if [ "$restored" -eq 1 ] && [ -f "$NEW_BACKUP/current-relay.sqlite3" ]; then
        rm -f "$INST/relay.sqlite3" "$INST/relay.sqlite3-wal" "$INST/relay.sqlite3-shm"
        cp "$NEW_BACKUP/current-relay.sqlite3" "$INST/relay.sqlite3" || restored=0
        chmod 600 "$INST/relay.sqlite3" 2>/dev/null || true
      fi
      if [ "$restored" -eq 1 ]; then
        if launchctl bootstrap "$DOMAIN" "$PLIST" && health "$APP"; then
          echo "已恢复到回退前状态" >&2
        else
          restored=0
        fi
      fi
    fi
  fi
  exit "$code"
}
trap fail_recovery EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

stop_service || fail "试用服务未在期限内停止，保留现场：$INST"
stopped=1

# 1. 为当前状态建立同格式备份（App + 数据快照 + metadata）。
NEW_BACKUP="$(mktemp -d "$DEST/.relay-backup.XXXXXX")"
"$APP/$BIN" maintenance snapshot "$NEW_BACKUP/data" --instance-dir "$INST" >/dev/null
CUR_APP_SHA="$(/usr/bin/shasum -a 256 "$APP/$BIN" | awk '{print $1}')"
CUR_SNAP_SHA="$(digest_dir "$NEW_BACKUP/data")"
cat > "$NEW_BACKUP/metadata.json" <<JSON
{"created_at":"$(date -u +%Y-%m-%dT%H:%M:%SZ)","product":"$CURRENT_VERSION","db_schema":$CURRENT_SCHEMA,"source":"rollback","app_sha256":"$CUR_APP_SHA","database_snapshot":"data","database_snapshot_sha256":"$CUR_SNAP_SHA"}
JSON
mv "$APP" "$NEW_BACKUP/RelayTaskWindow.app"

# 2. 恢复目标 App。
mv "$BACKUP/RelayTaskWindow.app" "$APP"

# 3. 恢复数据：同 schema 直接还原快照；9 → 8 只走既有 rollback-v8。
if [ "$CURRENT_SCHEMA" = "$TARGET_SCHEMA" ]; then
  mv "$INST/relay.sqlite3" "$NEW_BACKUP/current-relay.sqlite3"
  [ ! -f "$INST/relay.sqlite3-wal" ] || mv "$INST/relay.sqlite3-wal" "$NEW_BACKUP/current-relay.sqlite3-wal"
  [ ! -f "$INST/relay.sqlite3-shm" ] || mv "$INST/relay.sqlite3-shm" "$NEW_BACKUP/current-relay.sqlite3-shm"
  cp "$SNAP_DIR/relay.sqlite3" "$INST/relay.sqlite3.tmp"
  chmod 600 "$INST/relay.sqlite3.tmp"
  mv "$INST/relay.sqlite3.tmp" "$INST/relay.sqlite3"
  /bin/sync
elif [ "$CURRENT_SCHEMA" = "9" ] && [ "$TARGET_SCHEMA" = "8" ]; then
  [ -d "$BACKUP/database-exchange" ] || fail "缺少 v9→v8 交换目录，拒绝降级回退"
  "$APP/$BIN" maintenance rollback-v8 "$BACKUP/database-exchange" --instance-dir "$INST"
else
  fail "不支持的数据版本回退：$CURRENT_SCHEMA → $TARGET_SCHEMA"
fi

# 4. 包装脚本与 plist 与恢复后的 App 对齐，再启动并验收。
install -m 700 "$APP/Contents/Resources/relay/run-daemon.sh" "$ROOT/run-daemon.sh"
write_wrapper_plist
launchctl bootstrap "$DOMAIN" "$PLIST"
SERVICE_STARTED=1
health "$APP" || fail "回退后服务未通过 MCP 健康检查"
"$APP/$BIN" doctor --instance-dir "$INST" >/dev/null
"$APP/$BIN" maintenance check-idle --instance-dir "$INST" > "$NEW_BACKUP/started.json"
RESTORED_VERSION="$(read_plist CFBundleShortVersionString "$APP/Contents/Info.plist")"
[ "$RESTORED_VERSION" = "$TARGET_PRODUCT" ] || fail "回退后版本不匹配"
RESTORED_SCHEMA="$(current_schema)"
[ "$RESTORED_SCHEMA" = "$TARGET_SCHEMA" ] || fail "回退后 schema 不匹配"
printf '已回退到 %s（schema %s）；回退前状态备份：%s\n' "$TARGET_PRODUCT" "$TARGET_SCHEMA" "$NEW_BACKUP"
trap - EXIT
