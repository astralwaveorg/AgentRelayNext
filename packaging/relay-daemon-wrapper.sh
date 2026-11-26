#!/bin/bash
# Stable launchd entrypoint: rotate local logs before and during daemon runtime.
# Keeping this script outside the .app lets an upgrade/rollback keep the same
# LaunchAgent while the app bundle is replaced.
set -euo pipefail
ROOT="$1"; INSTANCE="$2"; CONFIG="$3"; RELAY="$4"
LIMIT=$((5 * 1024 * 1024))
shift_backups() {
  local file="$1" index
  for index in 4 3 2 1; do
    if [ -f "$file.$index" ]; then
      mv -f "$file.$index" "$file.$((index + 1))"
    fi
  done
}
rotate_startup() {
  local file="$1" size
  [ -f "$file" ] || return 0
  size="$(/usr/bin/stat -f%z "$file" 2>/dev/null || echo 0)"
  [ "$size" -gt "$LIMIT" ] || return 0
  shift_backups "$file"
  mv -f "$file" "$file.1"
}
# Copy-and-truncate keeps the daemon's open O_APPEND descriptor writing to
# $file at offset 0 after truncation, so subsequent rotations keep working.
rotate_live() {
  local file="$1" size
  [ -f "$file" ] || return 0
  size="$(/usr/bin/stat -f%z "$file" 2>/dev/null || echo 0)"
  [ "$size" -gt "$LIMIT" ] || return 0
  shift_backups "$file"
  cp -p "$file" "$file.1"
  : > "$file"
}
mkdir -p "$ROOT"
chmod 700 "$ROOT" 2>/dev/null || true
rotate_startup "$ROOT/daemon.log"
rotate_startup "$ROOT/daemon.err.log"
# Rotate while the daemon runs, not only at startup. The subshell watches the
# wrapper PID (which becomes the daemon after exec) and exits with it.
pid=$$
interval="${RELAY_LOG_ROTATE_SECONDS:-60}"
(
  while kill -0 "$pid" 2>/dev/null; do
    sleep "$interval"
    rotate_live "$ROOT/daemon.log"
    rotate_live "$ROOT/daemon.err.log"
  done
) &
exec "$RELAY" daemon --instance-dir "$INSTANCE" --config "$CONFIG" \
  >>"$ROOT/daemon.log" 2>>"$ROOT/daemon.err.log"
