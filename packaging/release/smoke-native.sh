#!/bin/bash
# Native arm64 release smoke: version -> prepare -> foreground daemon ->
# doctor -> mcp-verify -> offline demo -> artifact digests -> DMG mount.
#
# Usage: smoke-native.sh <RelayTaskWindow.app> [<RelayTaskWindow-*.dmg>]
#
# Everything runs under a temporary HOME in /private/tmp. No LaunchAgent is
# registered, no vendor CLI is called, and no network request is made.
set -euo pipefail
APP="$1"
DMG="${2:-}"
[ -d "$APP" ] && [ ! -L "$APP" ] || { echo "smoke: need a regular .app" >&2; exit 2; }
RELAY="$APP/Contents/Resources/relay/relayctl"
[ -x "$RELAY" ] || { echo "smoke: missing relayctl" >&2; exit 2; }
WRAPPER="$APP/Contents/Resources/relay/run-daemon.sh"
[ -x "$WRAPPER" ] || { echo "smoke: missing run-daemon.sh" >&2; exit 2; }

TMP="$(mktemp -d /private/tmp/relay-smoke.XXXXXX)"
export HOME="$TMP/home"
mkdir -p "$HOME"
ROOT="$HOME/.agent-relay-next"
SOCK="$ROOT/instance/relay.sock"
DPID=""
MOUNT=""
cleanup() {
  [ -n "$DPID" ] && kill "$DPID" 2>/dev/null || true
  if [ -n "$MOUNT" ]; then /usr/bin/hdiutil detach "$MOUNT" >/dev/null 2>&1 || true; fi
  rm -rf "$TMP"
}
trap cleanup EXIT
step() { echo "smoke: $*"; }

step "version"
"$RELAY" version | grep -q '"product"'

step "prepare (temp HOME)"
"$RELAY" local-preview prepare >/dev/null

step "start foreground daemon"
"$WRAPPER" "$ROOT" "$ROOT/instance" "$ROOT/config.json" "$RELAY" &
DPID=$!
for _ in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done
[ -S "$SOCK" ] || { echo "smoke: socket did not appear" >&2; exit 1; }

step "doctor"
"$RELAY" doctor --instance-dir "$ROOT/instance" >/dev/null

step "mcp-verify"
"$RELAY" mcp-verify --socket "$SOCK" >/dev/null

BEFORE_HEAD="$(/usr/bin/git -C "$ROOT/repo" rev-parse HEAD)"
BEFORE_STATUS="$(/usr/bin/git -C "$ROOT/repo" status --porcelain)"
step "offline demo submit"
SUBMIT="$("$RELAY" local-preview submit)"
TASK="$(printf '%s' "$SUBMIT" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])')"
[ -n "$TASK" ] || { echo "smoke: submit returned no task" >&2; exit 1; }

step "wait for terminal state"
DEADLINE=$(( $(date +%s) + 150 ))
STATE="unknown"
while :; do
  STATUS="$("$RELAY" --socket "$SOCK" status --task-id "$TASK")"
  STATE="$(printf '%s' "$STATUS" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["state"])')"
  case "$STATE" in
    queued|preparing|launching|running|verifying) ;;
    *) break ;;
  esac
  [ "$(date +%s)" -lt "$DEADLINE" ] || { echo "smoke: task timeout in $STATE" >&2; exit 1; }
  sleep 1
done
if [ "$STATE" = "awaiting_review" ]; then
  step "artifact digests"
  read -r DIFF_SHA RECEIPT_SHA < <(/usr/bin/python3 - "$RELAY" "$SOCK" "$TASK" <<'PY'
import hashlib, json, subprocess, sys
relay, sock, task = sys.argv[1], sys.argv[2], sys.argv[3]
def artifact(kind):
    out = subprocess.check_output([relay, "--socket", sock, "artifact",
                                   "--task-id", task, "--attempt-no", "1", "--kind", kind])
    return json.loads(out)["content"]
diff = artifact("diff").encode()
receipt = artifact("test_receipt").encode()
review = json.loads(artifact("handoff"))["workflow"]["review"]
assert review["candidate_diff_sha256"] == hashlib.sha256(diff).hexdigest()
assert review["test_receipt_sha256"] == hashlib.sha256(receipt).hexdigest()
print(hashlib.sha256(diff).hexdigest(), hashlib.sha256(receipt).hexdigest())
PY
)
  REV="$(printf '%s' "$STATUS" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["revision"])')"
  step "accept synthetic result (local human approval)"
  "$RELAY" --socket "$SOCK" review --task-id "$TASK" --attempt-no 1 \
    --expected-revision "$REV" --request-id "$(/usr/bin/uuidgen | tr 'A-Z' 'a-z')" \
    --verdict accept --diff-sha256 "$DIFF_SHA" --test-sha256 "$RECEIPT_SHA" >/dev/null
  for _ in $(seq 1 30); do
    STATUS="$("$RELAY" --socket "$SOCK" status --task-id "$TASK")"
    STATE="$(printf '%s' "$STATUS" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["state"])')"
    [ "$STATE" = "completed" ] && break
    sleep 1
  done
fi
[ "$STATE" = "completed" ] || { echo "smoke: task ended in $STATE" >&2; exit 1; }

step "main checkout unchanged"
[ "$(/usr/bin/git -C "$ROOT/repo" rev-parse HEAD)" = "$BEFORE_HEAD" ]
[ "$(/usr/bin/git -C "$ROOT/repo" status --porcelain)" = "$BEFORE_STATUS" ]

if [ -n "$DMG" ]; then
  step "mount DMG"
  MOUNT="$(mktemp -d /private/tmp/relay-smoke-dmg.XXXXXX)"
  /usr/bin/hdiutil attach -readonly -nobrowse -noautoopen -mountpoint "$MOUNT" "$DMG" >/dev/null
  "$MOUNT/RelayTaskWindow.app/Contents/Resources/relay/relayctl" version | grep -q '"product"'
  "$MOUNT/RelayTaskWindow.app/Contents/MacOS/RelayTaskWindow" --socket "$SOCK" --dump >/dev/null
  /usr/bin/hdiutil detach "$MOUNT" >/dev/null
  MOUNT=""
fi

kill "$DPID" 2>/dev/null || true
DPID=""
step "ok"
