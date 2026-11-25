#!/bin/bash
# Stable launchd entrypoint: rotate local logs before exec'ing the daemon.
# Keeping this script outside the .app lets an upgrade/rollback keep the same
# LaunchAgent while the app bundle is replaced.
set -euo pipefail
ROOT="$1"; INSTANCE="$2"; CONFIG="$3"; RELAY="$4"
LIMIT=$((5 * 1024 * 1024))
rotate() {
  local file="$1" size
  [ -f "$file" ] || return 0
  size="$(/usr/bin/stat -f%z "$file" 2>/dev/null || echo 0)"
  [ "$size" -gt "$LIMIT" ] || return 0
  local index
  for index in 4 3 2 1; do
    [ -f "$file.$index" ] && mv -f "$file.$index" "$file.$((index + 1))"
  done
  mv -f "$file" "$file.1"
}
mkdir -p "$ROOT"
chmod 700 "$ROOT" 2>/dev/null || true
rotate "$ROOT/daemon.log"
rotate "$ROOT/daemon.err.log"
# Rotate while the daemon runs, not only at startup. The subshell watches the
# wrapper PID (which becomes the daemon after exec) and exits with it.
pid=$$
interval="${RELAY_LOG_ROTATE_SECONDS:-60}"
(
  while kill -0 "$pid" 2>/dev/null; do
    sleep "$interval"
    rotate "$ROOT/daemon.log"
    rotate "$ROOT/daemon.err.log"
  done
) &
exec "$RELAY" daemon --instance-dir "$INSTANCE" --config "$CONFIG" \
  >>"$ROOT/daemon.log" 2>>"$ROOT/daemon.err.log"
