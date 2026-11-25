#!/bin/bash
# 本机试用升级，支持同 schema 和显式支持的 v8 → v9。
set -euo pipefail
umask 077
[ "$#" -eq 1 ] || { echo "用法：upgrade-local-preview.sh <zip>" >&2; exit 2; }
SCRIPT="$(cd "$(dirname "$0")" && pwd -P)"
DEST="$HOME/Applications"
APP="$DEST/RelayTaskWindow.app"
ROOT="$HOME/.agent-relay-next"
INST="$ROOT/instance"
LOCK="$ROOT/upgrade.lock"
FROZEN="$INST/upgrade.maintenance"
LABEL="${RELAY_PREVIEW_LABEL:-net.relay.candidate.local-preview}"
case "$LABEL" in net.relay.candidate.local-preview*) ;; *) exit 2;; esac
case "$LABEL" in *[!a-zA-Z0-9.-]*) exit 2;; esac
DOMAIN="gui/$(id -u)"
SERVICE="$DOMAIN/$LABEL"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
BIN="Contents/Resources/relay/relayctl"
INFO="Contents/Info.plist"
fail() { echo "$*" >&2; exit 1; }
read_plist() { /usr/bin/plutil -extract "$1" raw -o - "$2"; }

for p in "$DEST" "$APP" "$ROOT" "$INST" "$PLIST"; do [ ! -L "$p" ] || fail "拒绝符号链接：$p"; done
[ "$(cd "$ROOT" && pwd -P)" = "$ROOT" ] && [ "$(cd "$DEST" && pwd -P)" = "$DEST" ] || fail "试用路径不是物理路径"
[ "$(read_plist Label "$PLIST")" = "$LABEL" ] || fail "launchd 注册 Label 不匹配"
old_layout() {
  [ "$(read_plist ProgramArguments.0 "$PLIST")" = "$APP/$BIN" ] &&
  [ "$(read_plist ProgramArguments.1 "$PLIST")" = daemon ] &&
  [ "$(read_plist ProgramArguments.2 "$PLIST")" = --instance-dir ] &&
  [ "$(read_plist ProgramArguments.3 "$PLIST")" = "$INST" ] &&
  [ "$(read_plist ProgramArguments.4 "$PLIST")" = --config ] &&
  [ "$(read_plist ProgramArguments.5 "$PLIST")" = "$ROOT/config.json" ]
}
wrapper_layout() {
  [ "$(read_plist ProgramArguments.0 "$PLIST")" = "$ROOT/run-daemon.sh" ] &&
  [ "$(read_plist ProgramArguments.1 "$PLIST")" = "$ROOT" ] &&
  [ "$(read_plist ProgramArguments.2 "$PLIST")" = "$INST" ] &&
  [ "$(read_plist ProgramArguments.3 "$PLIST")" = "$ROOT/config.json" ] &&
  [ "$(read_plist ProgramArguments.4 "$PLIST")" = "$APP/$BIN" ]
}
old_layout || wrapper_layout || fail "launchd 注册与试用实例不匹配"
launchctl print "$SERVICE" >/dev/null || fail "试用服务未登记"
mkdir "$LOCK" || fail "已有升级锁，保留现场：$LOCK"

# Wait for launchd to drop the service AND the daemon to remove its socket.
# `launchctl bootout` returns before the process has necessarily finished its
# foreground cleanup; taking a snapshot too early hits daemon.lock.
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
  for _ in $(seq 1 25); do
    if [ -S "$INST/relay.sock" ] && "$1/$BIN" mcp-verify --socket "$INST/relay.sock"       > "$LOCK/mcp.tmp" 2>&1; then
      rm -f "$LOCK/mcp.tmp"
      return 0
    fi
    sleep 0.2
  done
  rm -f "$LOCK/mcp.tmp"
  return 1
}
write_wrapper_plist() {
  local temp
  temp="$(mktemp "$HOME/Library/LaunchAgents/.relay-upgrade.XXXXXX")"
  /usr/bin/plutil -create xml1 "$temp"
  /usr/bin/plutil -insert Label -string "$LABEL" "$temp"
  /usr/bin/plutil -insert ProgramArguments -json '[]' "$temp"
  local i=0 arg
  for arg in "$ROOT/run-daemon.sh" "$ROOT" "$INST" "$ROOT/config.json" "$APP/$BIN"; do
    /usr/bin/plutil -insert "ProgramArguments.$i" -string "$arg" "$temp"
    i=$((i+1))
  done
  /usr/bin/plutil -insert RunAtLoad -bool true "$temp"
  /usr/bin/plutil -insert KeepAlive -bool true "$temp"
  /usr/bin/plutil -lint "$temp" >/dev/null
  chmod 600 "$temp"
  mv -f "$temp" "$PLIST"
}

stage=""; backup=""; stopped=0; frozen=0; installed=0; restarting=0; exchanged=0
cleanup() {
  local code=$? restored=1
  trap - EXIT INT TERM
  set +e
  if [ "$code" -ne 0 ] && [ "$stopped" -eq 1 ]; then
    if [ "$restarting" -eq 1 ]; then
      stop_service || restored=0
    fi
    if [ "$restored" -eq 1 ] && [ "$exchanged" -eq 1 ]; then
      local helper="$NEXT/$BIN"
      if [ "$installed" -eq 1 ]; then helper="$APP/$BIN"; fi
      "$helper" maintenance rollback-v8 "$backup/database-exchange" --instance-dir "$INST" || restored=0
    fi
    if [ "$restored" -eq 1 ] && [ -n "$backup" ] && [ -d "$backup/RelayTaskWindow.app" ]; then
      if [ "$installed" -eq 1 ]; then
        mv "$APP" "$backup/failed-candidate.app" || restored=0
      fi
      if [ "$restored" -eq 1 ]; then mv "$backup/RelayTaskWindow.app" "$APP" || restored=0; fi
    fi
    if [ "$restored" -eq 1 ]; then
      if [ "$frozen" -eq 1 ]; then rm "$FROZEN" && frozen=0 || restored=0; fi
      if [ "$restored" -eq 1 ]; then
        if launchctl bootstrap "$DOMAIN" "$PLIST" && health "$APP"; then
          echo "升级未通过，旧版已恢复并通过启动检查。备份：${backup:-未建立}" >&2
        else
          restored=0
        fi
      fi
    fi
    [ "$restored" -eq 1 ] || echo "自动恢复未通过，停止继续操作；备份：${backup:-未建立}，锁：$LOCK" >&2
  fi
  if [ "$restored" -eq 1 ]; then
    if [ "$frozen" -eq 1 ]; then rm "$FROZEN"; fi
    if [ -n "$stage" ]; then rm -rf "$stage"; fi
    rmdir "$LOCK"
  fi
  exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
stage="$(mktemp -d "$DEST/.relay-upgrade.XXXXXX")"
"$SCRIPT/install.sh" "$1" "$stage"
NEXT="$stage/RelayTaskWindow.app"
OLD_VERSION="$(read_plist CFBundleShortVersionString "$APP/$INFO")"
NEW_VERSION="$(read_plist CFBundleShortVersionString "$NEXT/$INFO")"
[ "$OLD_VERSION" != "$NEW_VERSION" ] || fail "该入口要求跨版本更新"
NEXT_V="$("$NEXT/$BIN" version)"
printf '%s' "$NEXT_V" > "$stage/version.json"
field() { read_plist "$2" "$1"; }
[ "$(field "$stage/version.json" product)" = "$NEW_VERSION" ] || fail "应用与 relayctl 版本不一致"

# 0.1.0 不识别标记。首次升级时需先关闭任务窗口且暂停其他客户端提交。
# 后续版本进入维护后，连接级写命令和后台领取都会暂停。
"$NEXT/$BIN" maintenance check-idle --instance-dir "$INST" > "$stage/current.json"
for field_name in business wire; do
  [ "$(field "$stage/current.json" "$field_name")" = "$(field "$stage/version.json" "$field_name")" ] ||
    fail "业务或传输协议不兼容：$field_name"
done
[ "$(field "$stage/current.json" product)" = "$OLD_VERSION" ] || fail "服务与已安装应用版本不一致"

old_schema="$(field "$stage/current.json" db_schema)"
new_schema="$(field "$stage/version.json" db_schema)"
if [ "$old_schema" != "$new_schema" ]; then
  [ "$old_schema:$new_schema" = '8:9' ] || fail "不支持的 schema 升级：$old_schema → $new_schema"
fi

stop_service || fail "试用服务未在期限内停止，保留现场：$INST"
stopped=1
# 停服后复查：期间不排除有客户端刚好提交；确认空闲后再置维护标记，
# 避免"先置标记、后在途写"的窗口。
active="$(/usr/bin/sqlite3 "$INST/relay.sqlite3" "SELECT
  (SELECT count(*) FROM tasks WHERE state IN
      ('queued','preparing','launching','running','verifying','cancelling')) +
  (SELECT count(*) FROM dispatch_gate WHERE state='pending') +
  (SELECT count(*) FROM managed_groups WHERE state IN ('tracking','blocking'));")"
[ "$active" = "0" ] || fail "停服后仍有活动任务（$active），保留现场：$INST"
(set -o noclobber; : > "$FROZEN") || fail "维护标记已存在，保留现场"
frozen=1
backup="$(mktemp -d "$DEST/.relay-backup.XXXXXX")"
# Socket removal precedes the daemon's final lock release; retry the snapshot
# only on lock contention, and fail immediately on any other error.
snapshot_ok=0
for _ in $(seq 1 50); do
  if "$NEXT/$BIN" maintenance snapshot "$backup/data" --instance-dir "$INST" > "$stage/snapshot.log" 2>&1; then
    snapshot_ok=1
    break
  fi
  if ! grep -q '被其他进程持有' "$stage/snapshot.log"; then
    cat "$stage/snapshot.log" >&2
    fail "升级前数据快照失败"
  fi
  sleep 0.1
done
if [ "$snapshot_ok" -ne 1 ]; then
  cat "$stage/snapshot.log" >&2
  fail "等待 daemon 释放实例锁超时"
fi
# Unified backup metadata shared with install/rollback.
OLD_APP_SHA="$(/usr/bin/shasum -a 256 "$APP/$BIN" | awk '{print $1}')"
SNAPSHOT_SHA="$(cd "$backup/data" && find . -type f | LC_ALL=C sort | while IFS= read -r f; do
  /usr/bin/shasum -a 256 "$f"
done | /usr/bin/shasum -a 256 | awk '{print $1}')"
cat > "$backup/metadata.json" <<JSON
{"created_at":"$(date -u +%Y-%m-%dT%H:%M:%SZ)","product":"$OLD_VERSION","db_schema":$old_schema,"source":"upgrade","app_sha256":"$OLD_APP_SHA","database_snapshot":"data","database_snapshot_sha256":"$SNAPSHOT_SHA"}
JSON
if [ "$old_schema" != "$new_schema" ]; then
  "$NEXT/$BIN" maintenance prepare-v8-v9 "$backup/database-exchange" --instance-dir "$INST"
  exchanged=1
  "$NEXT/$BIN" maintenance activate-v9 "$backup/database-exchange" --instance-dir "$INST"
fi
mv "$APP" "$backup/RelayTaskWindow.app"
mv "$NEXT" "$APP"
installed=1
# Stable wrapper outside the app rotates 5 MiB x 5 logs before exec. Migrate
# the old direct-daemon LaunchAgent atomically; the wrapper remains valid if
# the candidate fails and the previous app bundle is restored.
install -m 700 "$APP/Contents/Resources/relay/run-daemon.sh" "$ROOT/run-daemon.sh"
write_wrapper_plist
# 候选服务保持维护，先验证可连、运行的不是旧版，并握手检查 MCP。
restarting=1
launchctl bootstrap "$DOMAIN" "$PLIST"
health "$APP" || fail "候选服务未通过 MCP 健康检查"
"$APP/$BIN" maintenance check-idle --instance-dir "$INST" > "$stage/started.json"
[ "$(field "$stage/started.json" product)" = "$NEW_VERSION" ] &&
[ "$(field "$stage/started.json" db_schema)" = "$new_schema" ] || fail "实际 daemon 版本不匹配"
"$APP/$BIN" doctor --instance-dir "$INST"
rm "$FROZEN"
frozen=0
printf '已升级 %s → %s；原应用和已核对的数据快照：%s\n' "$OLD_VERSION" "$NEW_VERSION" "$backup"
