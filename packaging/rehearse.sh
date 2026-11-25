#!/bin/bash
# 安装预演（一条可重跑命令）：打包 → 临时目录安装 → 用安装包内的 daemon 跑
# 离线样本 → 任务窗口读同一实例 → 升级（重装）→ 回滚 → 数据备份恢复核对。
# 全程在临时目录；不安装到正式位置，不签名/公证/发布，不启用真实模型。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/rr.XXXXXX")"   # 短路径：socket 受 sun_path 限制
# 退出码必须保留：trap 里的最后一条命令会覆盖 $?，失败会被报成成功。
# RELAY_KEEP=1：保留工作目录便于排查（失败时先看 daemon.log）。
trap 'code=$?; pkill -f "relayctl daemon --instance-dir $WORK" 2>/dev/null || true; if [ -n "${RELAY_KEEP:-}" ]; then echo "工作目录保留：$WORK"; else rm -rf "$WORK"; echo "工作目录已清理"; fi; exit $code' EXIT
step() { echo; echo "=== $* ==="; }
expect_ok() { "$@" >/dev/null 2>&1 || { echo "FAIL: $* (exit $?)" >&2; exit 1; }; echo "ok: $*"; }
expect_out() { # expect_out <needle> <cmd...>
  # 形参用位置变量直接取：函数内不声明局部同名变量，避免与 set -u 冲突。
  local want="$1"; shift
  local out=""
  if ! out="$("$@" 2>&1)"; then
    echo "FAIL: $* (exit $?)" >&2; echo "$out" >&2; exit 1
  fi
  case "$out" in
    *"$want"*) echo "ok: $* → 含「${want}」";;
    *) echo "FAIL: $* 输出不含「${want}」:" >&2; echo "$out" >&2; exit 1;;
  esac
}

step "1 打包"
"$ROOT/packaging/package.sh" "$WORK/dist"
ZIP="$(ls -1 "$WORK/dist"/*.zip)"
step "2 临时目录安装"
expect_ok "$ROOT/packaging/install.sh" "$ZIP" "$WORK/install"
APP="$WORK/install/RelayTaskWindow.app"; RELAY="$APP/Contents/Resources/relay/relayctl"
expect_ok test -x "$RELAY"
expect_out relay.v1 "$RELAY" version

step "3 安装包内的 daemon 跑同一离线样本"
INST="$WORK/inst"; mkdir -m 700 "$INST" "$WORK/managed"
mkdir -p "$WORK/repo/src" "$WORK/repo/tests"
printf 'pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n' > "$WORK/repo/src/count.rs"
printf '// placeholder\n' > "$WORK/repo/tests/count.rs"
git -C "$WORK/repo" init -q -b main; git -C "$WORK/repo" add -A
git -C "$WORK/repo" -c user.name=rr -c user.email=rr@example.invalid commit -q -m baseline
HEAD="$(git -C "$WORK/repo" rev-parse HEAD)"
printf '#!/bin/sh\nexit 0\n' > "$WORK/rule.sh"; chmod +x "$WORK/rule.sh"
cat > "$WORK/config.json" <<JSON
{"repo_root": "$WORK/repo", "managed_root": "$WORK/managed",
 "adapter_catalog_dir": "$APP/Contents/Resources/relay/mock/catalog",
 "implement_env": {"IMPLEMENT_MODE": "rework-once"},
 "worker_timeout_ms": 20000,
 "rules": [{"id": "mock_rule", "executable": "$WORK/rule.sh", "argv": [], "timeout_ms": 5000}]}
JSON
"$RELAY" daemon --instance-dir "$INST" --config "$WORK/config.json" > "$WORK/daemon.log" 2>&1 &
for _ in $(seq 1 100); do [ -S "$INST/relay.sock" ] && break; sleep 0.1; done
S="$INST/relay.sock"; R() { "$RELAY" --socket "$S" "$@"; }
PLAN="$(R plan --project-id proj_rr --goal 安装预演样本 --brief b --scope src/count.rs tests/count.rs --profile-id prof_auto --rules mock_rule --base-oid "$HEAD")"
PID_PLAN="$(printf '%s' "$PLAN" | sed -n 's/.*"plan_id": *"\([^"]*\)".*/\1/p' | head -1)"
[ -n "$PID_PLAN" ] || { echo "plan 失败: $PLAN" >&2; exit 1; }
TASK="$(R submit --plan-id "$PID_PLAN" --request-id 5eed0000-0000-4000-8000-000000000001)"
TASK_ID="$(printf '%s' "$TASK" | sed -n 's/.*"id": *"\(task_[^"]*\)".*/\1/p' | head -1)"
[ -n "$TASK_ID" ] || { echo "submit 失败: $TASK" >&2; exit 1; }
for _ in $(seq 1 800); do
  STATE="$(R status --task-id "$TASK_ID" | sed -n 's/.*"state": *"\([^"]*\)".*/\1/p' | head -1)"
  case "$STATE" in awaiting_review|failed) break;; esac; sleep 0.25
done
[ "$STATE" = awaiting_review ] || { echo "任务未到待验收: $STATE" >&2; cat "$WORK/daemon.log" >&2; exit 1; }
echo "ok: 任务 $TASK_ID 到待验收"
expect_out pass R artifact --task-id "$TASK_ID" --attempt-no 1 --kind handoff
expect_ok "$RELAY" doctor --instance-dir "$INST"
expect_ok "$RELAY" mcp-verify --socket "$S"

step "4 任务窗口读同一实例（无界面模式）"
expect_out 待验收 "$APP/Contents/MacOS/RelayTaskWindow" --socket "$S" --dump
expect_out "返工 1 次" "$APP/Contents/MacOS/RelayTaskWindow" --socket "$S" --dump

step "5 升级（重装）→ 数据与状态保持 → 统一备份格式"
expect_ok "$ROOT/packaging/install.sh" "$ZIP" "$WORK/install"
expect_out awaiting_review R status --task-id "$TASK_ID"   # 重装后旧实例的状态与制品仍可读
pkill -f "relayctl daemon --instance-dir $INST" || true; sleep 0.5
"$RELAY" daemon --instance-dir "$INST" --config "$WORK/config.json" >> "$WORK/daemon.log" 2>&1 &
for _ in $(seq 1 100); do [ -S "$S" ] && break; sleep 0.1; done
expect_out awaiting_review R status --task-id "$TASK_ID"
# 重装必须在统一 .relay-backup.* 中留下带 metadata 的旧应用备份。
BACKUP_DIR="$(ls -1d "$WORK/install"/.relay-backup.*/ | head -1)"
expect_ok test -f "$BACKUP_DIR/metadata.json"
expect_ok test -d "$BACKUP_DIR/RelayTaskWindow.app"
expect_out '"source":"install"' cat "$BACKUP_DIR/metadata.json"
pkill -f "relayctl daemon --instance-dir $INST" || true; sleep 0.5

step "6 数据备份与恢复核对（一致快照 → 新临时实例）"
sqlite3 "$INST/relay.sqlite3" "VACUUM INTO '$WORK/backup.sqlite3'"
SCHEMA_B="$(sqlite3 "$WORK/backup.sqlite3" "SELECT value FROM meta WHERE key='schema_version'")"
SCHEMA_L="$(sqlite3 "$INST/relay.sqlite3" "SELECT value FROM meta WHERE key='schema_version'")"
TASKS_B="$(sqlite3 "$WORK/backup.sqlite3" "SELECT COUNT(*) FROM tasks")"
EVENTS_B="$(sqlite3 "$WORK/backup.sqlite3" "SELECT COUNT(*) FROM events")"
EVENTS_L="$(sqlite3 "$INST/relay.sqlite3" "SELECT COUNT(*) FROM events")"
[ "$SCHEMA_B" = "$SCHEMA_L" ] && [ "$TASKS_B" = 1 ] && [ "$EVENTS_B" = "$EVENTS_L" ] \
  || { echo "备份核对失败 schema=$SCHEMA_B/$SCHEMA_L tasks=$TASKS_B events=$EVENTS_B/$EVENTS_L" >&2; exit 1; }
echo "ok: 备份 schema=${SCHEMA_B}，tasks=${TASKS_B}，events=$EVENTS_B 与源一致"
mkdir -m 700 "$WORK/restored"; cp "$WORK/backup.sqlite3" "$WORK/restored/relay.sqlite3"
expect_ok "$RELAY" doctor --instance-dir "$WORK/restored"
"$RELAY" daemon --instance-dir "$WORK/restored" --config "$WORK/config.json" > "$WORK/restored.log" 2>&1 &
for _ in $(seq 1 100); do [ -S "$WORK/restored/relay.sock" ] && break; sleep 0.1; done
expect_out awaiting_review "$RELAY" --socket "$WORK/restored/relay.sock" status --task-id "$TASK_ID"
expect_out "diff --git" "$RELAY" --socket "$WORK/restored/relay.sock" artifact --task-id "$TASK_ID" --attempt-no 1 --kind diff
pkill -f "relayctl daemon --instance-dir $WORK/restored" || true
echo; echo "安装预演通过：打包、临时安装、daemon 跑样本、任务窗口、重装、统一备份、备份恢复"
