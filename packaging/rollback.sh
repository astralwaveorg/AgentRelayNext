#!/bin/bash
# 回滚预演：把最近一次安装备份的应用恢复回来，并核对可执行文件摘要。
# 用法：rollback.sh <dest-root> [backup-stamp]
set -euo pipefail
DEST="$1"
if [ -n "${2:-}" ]; then
  case "$2" in *[!0-9]*) echo "无效的备份编号" >&2; exit 2;; esac
  BACKUP="$DEST/.backup/$2"
else
  BACKUP="$(ls -1d "$DEST/.backup"/*/ 2>/dev/null | sort | tail -1)"
fi
[ -d "$BACKUP/RelayTaskWindow.app" ] || { echo "没有可回滚的备份（${BACKUP}）" >&2; exit 1; }
BACKUP="${BACKUP%/}"
APP="$DEST/RelayTaskWindow.app"
STAGE="$(mktemp -d "$DEST/.relay-rollback.XXXXXX")"
cleanup() {
  local code=$?
  if [ "$code" -ne 0 ] && [ -d "$STAGE/current.app" ]; then
    if [ -e "$APP" ] || ! mv "$STAGE/current.app" "$APP"; then
      echo "无法恢复原应用；原应用保留在 $STAGE/current.app" >&2
      return "$code"
    fi
  fi
  rm -rf "$STAGE"
  return "$code"
}
trap cleanup EXIT
CANDIDATE="$STAGE/RelayTaskWindow.app"
ditto "$BACKUP/RelayTaskWindow.app" "$CANDIDATE"
(cd "$CANDIDATE/Contents/Resources/relay" && shasum -a 256 -c --status SHA256)
(cd "$CANDIDATE/Contents/MacOS" && shasum -a 256 -c --status SHA256)
VERSION="$("$CANDIDATE/Contents/Resources/relay/relayctl" version)"
if [ -e "$APP" ]; then mv "$APP" "$STAGE/current.app"; fi
mv "$CANDIDATE" "$APP"
echo "relayctl 与任务窗口摘要均与安装清单一致"
echo "已回滚到 $BACKUP"
printf '%s\n' "$VERSION"
