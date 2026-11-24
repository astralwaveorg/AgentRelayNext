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
  STAMP="$(date -u +%Y%m%d%H%M%S)"
  mkdir -p "$DEST/.backup"
  BACKUP="$DEST/.backup/$STAMP"
  mkdir "$BACKUP"
  mv "$APP" "$BACKUP/RelayTaskWindow.app"
fi
mv "$CANDIDATE" "$APP"
echo "已安装到 $APP"
printf 'bundle: %s\n%s\n' "$BUNDLE" "$VERSION"
