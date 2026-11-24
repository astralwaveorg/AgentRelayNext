#!/bin/bash
# 非覆盖恢复：独立档案读取、拒绝写请求、篡改检测。
set -euo pipefail
umask 077
[ "$#" -eq 2 ] || { echo "用法：rehearse-restore.sh <relayctl> <备份 data 目录>" >&2; exit 2; }
BIN="$1"
BACKUP="$2"
WORK="$(mktemp -d /private/tmp/rrs.XXXXXX)"
pid=""
cleanup() {
  local code=$?
  if [ -n "$pid" ]; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  if [ "$code" -eq 0 ] && [ -z "${RELAY_KEEP:-}" ]; then rm -rf "$WORK"; else echo "演练现场：$WORK" >&2; fi
}
trap cleanup EXIT
ARCHIVE="$WORK/archive"
INST="$ARCHIVE/instance"
SOCK="$INST/relay.sock"
DB="$INST/relay.sqlite3"
run() { "$BIN" --socket "$SOCK" "$@"; }

"$BIN" maintenance restore "$BACKUP" "$ARCHIVE"
task="$(/usr/bin/sqlite3 -readonly "$DB" "SELECT task_id FROM artifacts ORDER BY task_id LIMIT 1")"
attempt="$(/usr/bin/sqlite3 -readonly "$DB" "SELECT attempt_no FROM artifacts WHERE task_id='$task' ORDER BY attempt_no LIMIT 1")"
ref="$(/usr/bin/sqlite3 -readonly "$DB" "SELECT artifact_dir FROM artifacts WHERE task_id='$task' AND attempt_no=$attempt")"
[ "$ref" = "$ARCHIVE/managed/artifacts/$task-$attempt" ]
cmp "$ref/diff.patch" "$BACKUP/artifacts/$task-$attempt/diff.patch"
"$BIN" maintenance serve-archive --instance-dir "$INST" > "$WORK/server.log" 2>&1 &
pid=$!
for _ in $(seq 1 60); do
  if run status --task-id "$task" > "$WORK/task.json" 2>/dev/null; then break; fi
  kill -0 "$pid" 2>/dev/null || { cat "$WORK/server.log" >&2; exit 1; }
  sleep 0.1
done
grep -q "$task" "$WORK/task.json"
for kind in diff manifest test_receipt handoff; do
  run artifact --task-id "$task" --attempt-no "$attempt" --kind "$kind" >/dev/null
done
before="$(/usr/bin/sqlite3 -readonly "$DB" 'SELECT count(*) FROM events')"
if run submit --plan-id unused --request-id 01234567-0000-4000-8000-000000000000 > "$WORK/write.log" 2>&1; then exit 1; fi
grep -q SERVICE_UNAVAILABLE "$WORK/write.log"
[ "$before" = "$(/usr/bin/sqlite3 -readonly "$DB" 'SELECT count(*) FROM events')" ]
kill "$pid"
wait "$pid" 2>/dev/null || true
pid=""
"$BIN" maintenance snapshot "$WORK/new-backup" --instance-dir "$INST"
"$BIN" maintenance restore "$WORK/new-backup" "$WORK/archive2" > "$WORK/new-restore.json"
/usr/bin/grep -q '"database_sha256_verified":true' "$WORK/new-restore.json"
if "$BIN" maintenance restore "$BACKUP" "$ARCHIVE" > "$WORK/collision.log" 2>&1; then exit 1; fi
grep -q '恢复目标已存在' "$WORK/collision.log"
printf '\nchanged\n' >> "$WORK/new-backup/artifacts/$task-$attempt/diff.patch"
if "$BIN" maintenance restore "$WORK/new-backup" "$WORK/bad" > "$WORK/bad.log" 2>&1; then exit 1; fi
grep -q '制品摘要或长度不符' "$WORK/bad.log"
test ! -e "$WORK/bad"
# 摘要门须在 SQLite 解析前拒绝字节改动。
printf 'tampered' >> "$WORK/new-backup/relay.sqlite3"
if "$BIN" maintenance restore "$WORK/new-backup" "$WORK/bad-db" > "$WORK/bad-db.log" 2>&1; then exit 1; fi
grep -q '备份数据库摘要不匹配' "$WORK/bad-db.log"
test ! -e "$WORK/bad-db"
echo "独立恢复通过：原历史与四份制品可读，拒绝写入、覆盖及两类篡改。"
