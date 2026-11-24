#!/bin/bash
# First install of the offline preview, in the current macOS user's login session.
# Usage: install-local-preview.sh <zip>
set -euo pipefail
umask 077
SCRIPT="$(cd "$(dirname "$0")" && pwd -P)"
APP="$HOME/Applications/RelayTaskWindow.app"
ROOT="$HOME/.agent-relay-next"
LABEL="${RELAY_PREVIEW_LABEL:-net.relay.candidate.local-preview}"
case "$LABEL" in net.relay.candidate.local-preview*) ;; *) exit 2;; esac
case "$LABEL" in *[!a-zA-Z0-9.-]*) exit 2;; esac
DOMAIN="gui/$(id -u)"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
SOCKET="$ROOT/instance/relay.sock"
[ "${#SOCKET}" -lt 104 ] || { echo "本机演示 socket 路径过长" >&2; exit 1; }
for path in "$ROOT" "$PLIST" "$APP"; do
  if [ -e "$path" ] || [ -L "$path" ]; then echo "已有试用安装或数据，保留 $path 并停止" >&2; exit 1; fi
done
launchctl print "$DOMAIN" >/dev/null
if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then echo "演示服务已登记" >&2; exit 1; fi
"$SCRIPT/install.sh" "$1" "$HOME/Applications" --local-preview
RELAY="$APP/Contents/Resources/relay/relayctl"
"$RELAY" local-preview prepare
install -m 700 "$APP/Contents/Resources/relay/run-daemon.sh" "$ROOT/run-daemon.sh"
mkdir -p "$HOME/Library/LaunchAgents"
# plutil escapes user paths without embedding them in XML.
TEMP="$(mktemp "$HOME/Library/LaunchAgents/.relay-preview.XXXXXX")"
trap 'rm -f "$TEMP"' EXIT
/usr/bin/plutil -create xml1 "$TEMP"
/usr/bin/plutil -insert Label -string "$LABEL" "$TEMP"
/usr/bin/plutil -insert ProgramArguments -json '[]' "$TEMP"
i=0
for arg in "$ROOT/run-daemon.sh" "$ROOT" "$ROOT/instance" "$ROOT/config.json" "$RELAY"; do
  /usr/bin/plutil -insert "ProgramArguments.$i" -string "$arg" "$TEMP"
  i=$((i+1))
done
/usr/bin/plutil -insert RunAtLoad -bool true "$TEMP"
/usr/bin/plutil -insert KeepAlive -bool true "$TEMP"
/usr/bin/plutil -lint "$TEMP"
# Link without replacing an existing registration; both files share a parent.
ln "$TEMP" "$PLIST"
launchctl bootstrap "$DOMAIN" "$PLIST"
for _ in $(seq 1 60); do
  if [ -S "$SOCKET" ] && "$APP/Contents/MacOS/RelayTaskWindow" --socket "$SOCKET" --dump > "$ROOT/install-evidence.txt" 2>&1; then
    cat "$ROOT/install-evidence.txt"
    exit 0
  fi
  sleep 0.25
done
launchctl bootout "$DOMAIN/$LABEL" || true
echo "演示服务未就绪，保留安装现场：$ROOT" >&2
exit 1
