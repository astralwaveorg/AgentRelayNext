#!/bin/bash
# Install a locally built package. Only explicit --local-preview may target ~/Applications.
# Usage: install.sh <zip> <dest-root> [--local-preview]
set -euo pipefail
ZIP="$1"; DEST="$2"; MODE="${3:-}"
[ "$#" -le 3 ] && { [ -z "$MODE" ] || [ "$MODE" = --local-preview ]; }
case "$DEST" in /*) ;; *) echo "目标目录必须是绝对路径" >&2; exit 2;; esac
if [ "$MODE" = --local-preview ]; then
  [ "$DEST" = "$HOME/Applications" ] && [ ! -L "$DEST" ] || { echo "试用版只安装到当前用户的 Applications" >&2; exit 2; }
fi
case "$(cd "$DEST" 2>/dev/null && pwd -P || printf '%s' "$DEST")" in
  /Applications|/Applications/*|/System|/System/*) echo "拒绝：系统安装位置" >&2; exit 2;;
  "$HOME/Applications") [ "$MODE" = --local-preview ] || { echo "请使用 --local-preview 明确安装到本机用户目录" >&2; exit 2; };;
esac
[ -f "$ZIP" ] && [ -f "$ZIP.sha256" ] || { echo "安装包或 SHA-256 清单缺失" >&2; exit 1; }
# Read the expected hash only. Never trust a manifest to select paths to read.
read -r EXPECTED _ < "$ZIP.sha256"
ACTUAL="$(shasum -a 256 < "$ZIP")"; ACTUAL="${ACTUAL%% *}"
[ "$EXPECTED" = "$ACTUAL" ] || { echo "安装包摘要不一致" >&2; exit 1; }
mkdir -p "$DEST"
STAGE="$(mktemp -d "$DEST/.relay-install.XXXXXX")"
APP="$DEST/RelayTaskWindow.app"
BACKUP=""
cleanup() {
  local code=$?
  if [ "$code" -ne 0 ] && [ -n "$BACKUP" ] && [ -d "$BACKUP/RelayTaskWindow.app" ]; then
    if [ -e "$APP" ] || ! mv "$BACKUP/RelayTaskWindow.app" "$APP"; then
      echo "原应用保留在 $BACKUP，未能自动恢复" >&2
    fi
  fi
  rm -rf "$STAGE"
  return "$code"
}
trap cleanup EXIT
ditto -x -k "$ZIP" "$STAGE"
CANDIDATE="$STAGE/RelayTaskWindow.app"
[ -d "$CANDIDATE" ] && [ ! -L "$CANDIDATE" ] && [ ! -L "$APP" ]
RELAY_DIR="$CANDIDATE/Contents/Resources/relay"
UI_DIR="$CANDIDATE/Contents/MacOS"
(cd "$RELAY_DIR" && shasum -a 256 -c --status SHA256)
(cd "$UI_DIR" && shasum -a 256 -c --status SHA256)
VERSION="$("$RELAY_DIR/relayctl" version)"
BUNDLE="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$CANDIDATE/Contents/Info.plist")"
if [ -e "$APP" ]; then
  # Preview install is fresh only; upgrading a running launchd service is a separate operation.
  [ "$MODE" != --local-preview ] || { echo "已安装应用，拒绝覆盖运行中的试用版" >&2; exit 1; }
  # Unified backup layout shared with upgrade/rollback (.relay-backup.XXXXXX).
  BACKUP="$(mktemp -d "$DEST/.relay-backup.XXXXXX")"
  OLD_APP_SHA="$(/usr/bin/shasum -a 256 "$APP/Contents/Resources/relay/relayctl" | awk '{print $1}')"
  OLD_PRODUCT="$(sed -n 's/.*"product":"\([^"]*\)".*/\1/p' "$APP/Contents/Resources/relay/version.json" 2>/dev/null || true)"
  OLD_SCHEMA="$(sed -n 's/.*"db_schema":\([0-9]*\).*/\1/p' "$APP/Contents/Resources/relay/version.json" 2>/dev/null || echo 0)"
  cat > "$BACKUP/metadata.json" <<JSON
{"created_at":"$(date -u +%Y-%m-%dT%H:%M:%SZ)","product":"$OLD_PRODUCT","db_schema":${OLD_SCHEMA:-0},"source":"install","app_sha256":"$OLD_APP_SHA","database_snapshot":null,"database_snapshot_sha256":null}
JSON
  mv "$APP" "$BACKUP/RelayTaskWindow.app"
fi
mv "$CANDIDATE" "$APP"
echo "已安装到 $APP"
printf 'bundle: %s\n%s\n' "$BUNDLE" "$VERSION"
