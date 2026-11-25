#!/bin/bash
# 回退预演：临时 HOME + 唯一 launchd label，不接触本机安装或真实数据。
# 安装 → 建立统一备份 → 产生新事件 → rollback.sh → 校验数据/服务恢复。
set -euo pipefail
SCRIPT="$(cd "$(dirname "$0")" && pwd -P)"
WORK="$(mktemp -d /private/tmp/rrb.XXXXXX)"
REAL_HOME="$HOME"
HOME_OVERRIDDEN=""
cleanup() {
  local code=$?
  if [ -n "$HOME_OVERRIDDEN" ]; then
    launchctl bootout "gui/$(id -u)/$RELAY_PREVIEW_LABEL" >/dev/null 2>&1 || true
  fi
  if [ "$code" -ne 0 ] || [ -n "${RELAY_KEEP:-}" ]; then
    echo "回退预演现场：$WORK" >&2
  else
    rm -rf "$WORK"
  fi
  exit "$code"
}
trap cleanup EXIT

# 1. 打包（使用真实 HOME，打包本身不写用户目录）。
"$SCRIPT/package.sh" "$WORK/dist" >/dev/null
ZIP="$(ls -1 "$WORK/dist"/*.zip | head -1)"

# 2. 临时 HOME 安装（唯一 label，真实 launchd gui 域）。
export HOME="$WORK/home"
mkdir -p "$HOME"
HOME_OVERRIDDEN=1
export RELAY_PREVIEW_LABEL="net.relay.candidate.local-preview.qa.${WORK##*.}"
"$SCRIPT/install-local-preview.sh" "$ZIP" >/dev/null
APP="$HOME/Applications/RelayTaskWindow.app"
RELAY="$APP/Contents/Resources/relay/relayctl"
ROOT="$HOME/.agent-relay-next"
INST="$ROOT/instance"
SOCK="$INST/relay.sock"
LABEL="$RELAY_PREVIEW_LABEL"
DOMAIN="gui/$(id -u)"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

# 3. 一个任务进入待验收。
"$RELAY" local-preview submit > "$WORK/submit.json"
TASK="$(/usr/bin/plutil -extract task.id raw -o - "$WORK/submit.json")"
for _ in $(seq 1 300); do
  "$RELAY" --socket "$SOCK" status --task-id "$TASK" > "$WORK/task.json"
  [ "$(/usr/bin/plutil -extract task.state raw -o - "$WORK/task.json")" = awaiting_review ] && break
  sleep 0.1
done
[ "$(/usr/bin/plutil -extract task.state raw -o - "$WORK/task.json")" = awaiting_review ]

# 4. 停服 → 手工建立统一备份（同 upgrade 布局）→ 重启。
launchctl bootout "$DOMAIN/$LABEL"
for _ in $(seq 1 100); do [ ! -S "$SOCK" ] && break; sleep 0.1; done
BACKUP="$HOME/Applications/.relay-backup.manual"
mkdir -p "$BACKUP"
"$RELAY" maintenance snapshot "$BACKUP/data" --instance-dir "$INST" >/dev/null
APP_SHA="$(/usr/bin/shasum -a 256 "$RELAY" | awk '{print $1}')"
SNAP_SHA="$(cd "$BACKUP/data" && find . -type f | LC_ALL=C sort | while IFS= read -r f; do
  /usr/bin/shasum -a 256 "$f"
done | /usr/bin/shasum -a 256 | awk '{print $1}')"
PRODUCT="$(/usr/bin/plutil -extract CFBundleShortVersionString raw -o - "$APP/Contents/Info.plist")"
SCHEMA="$(/usr/bin/sqlite3 "$INST/relay.sqlite3" "select value from meta where key='schema_version';")"
cat > "$BACKUP/metadata.json" <<JSON
{"created_at":"$(date -u +%Y-%m-%dT%H:%M:%SZ)","product":"$PRODUCT","db_schema":$SCHEMA,"source":"rehearse","app_sha256":"$APP_SHA","database_snapshot":"data","database_snapshot_sha256":"$SNAP_SHA"}
JSON
ditto "$APP" "$BACKUP/RelayTaskWindow.app"
BEFORE_EVENTS="$(/usr/bin/sqlite3 -readonly "$INST/relay.sqlite3" 'select count(*) from events')"
BEFORE_STATE="$(/usr/bin/sqlite3 -readonly "$INST/relay.sqlite3" "select state from tasks where task_id='$TASK'")"
launchctl bootstrap "$DOMAIN" "$PLIST"
for _ in $(seq 1 100); do [ -S "$SOCK" ] && break; sleep 0.1; done

# 5. 人工接受该任务（产生新事件且不留下排队/活动状态），证明回退确实
#    还原了数据快照。
read -r DIFF_SHA RECEIPT_SHA < <(/usr/bin/python3 - "$RELAY" "$SOCK" "$TASK" <<'PY'
import hashlib, json, subprocess, sys
relay, sock, task = sys.argv[1], sys.argv[2], sys.argv[3]
def artifact(kind):
    out = subprocess.check_output([relay, "--socket", sock, "artifact",
                                   "--task-id", task, "--attempt-no", "1", "--kind", kind])
    return json.loads(out)["content"]
diff = artifact("diff").encode()
receipt = artifact("test_receipt").encode()
print(hashlib.sha256(diff).hexdigest(), hashlib.sha256(receipt).hexdigest())
PY
)
REV="$(/usr/bin/plutil -extract task.revision raw -o - "$WORK/task.json")"
"$RELAY" --socket "$SOCK" review --task-id "$TASK" --attempt-no 1 \
  --expected-revision "$REV" --request-id "$(/usr/bin/uuidgen | tr 'A-Z' 'a-z')" \
  --verdict accept --diff-sha256 "$DIFF_SHA" --test-sha256 "$RECEIPT_SHA" >/dev/null
for _ in $(seq 1 100); do
  "$RELAY" --socket "$SOCK" status --task-id "$TASK" > "$WORK/task2.json"
  [ "$(/usr/bin/plutil -extract task.state raw -o - "$WORK/task2.json")" = completed ] && break
  sleep 0.1
done
[ "$(/usr/bin/plutil -extract task.state raw -o - "$WORK/task2.json")" = completed ]
AFTER_EVENTS="$(/usr/bin/sqlite3 -readonly "$INST/relay.sqlite3" 'select count(*) from events')"
[ "$AFTER_EVENTS" -gt "$BEFORE_EVENTS" ] || {
  echo "预演未产生新增事件" >&2
  exit 1
}

# 6. 回退并校验。
"$SCRIPT/rollback.sh" "$HOME/Applications" manual > "$WORK/rollback.log"
grep -q '已回退到' "$WORK/rollback.log"
NEW_EVENTS="$(/usr/bin/sqlite3 -readonly "$INST/relay.sqlite3" 'select count(*) from events')"
[ "$NEW_EVENTS" = "$BEFORE_EVENTS" ] || {
  echo "回退后事件数不符：$NEW_EVENTS != $BEFORE_EVENTS" >&2
  exit 1
}
[ "$(/usr/bin/sqlite3 -readonly "$INST/relay.sqlite3" "select state from tasks where task_id='$TASK'")" = "$BEFORE_STATE" ]
"$RELAY" doctor --instance-dir "$INST" >/dev/null
"$RELAY" --socket "$SOCK" status --task-id "$TASK" >/dev/null
PRE_ROLLBACK="$(ls -1d "$HOME/Applications"/.relay-backup.*/ | grep -v '\.manual/$' | head -1)"
[ -f "$PRE_ROLLBACK/metadata.json" ]
grep -q '"source":"rollback"' "$PRE_ROLLBACK/metadata.json"
echo "回退预演通过：数据快照还原、服务健康、回退前状态已备份"
