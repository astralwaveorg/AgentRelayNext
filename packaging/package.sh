#!/bin/bash
# 本地打包预演：release 的 relayctl + 任务窗口 + 离线适配器目录 →
# RelayTaskWindow.app + zip + SHA-256 + manifest。不签名、不公证、不发布。
# 用法：package.sh [out-dir]      环境变量：RELAY_VERSION、RELAY_BUILD
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
VERSION="${RELAY_VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/crates/relay-daemon/Cargo.toml")}"
BUILD="${RELAY_BUILD:-$(date -u +%Y%m%d%H%M%S)}"
cd "$ROOT"
cargo build --release --locked --offline -p relay-daemon --bin relayctl
APP="$OUT/RelayTaskWindow.app"
rm -rf "$APP"
"$ROOT/packaging/build-app.sh" "$APP" "$VERSION" "$BUILD"
RELAY="$APP/Contents/Resources/relay"
mkdir -p "$RELAY/mock"
cp target/release/relayctl "$RELAY/relayctl"
cp -R crates/relay-daemon/mock/. "$RELAY/mock/"
mkdir "$RELAY/paired-demo"
cp packaging/qa/claude-review-e2e.py packaging/qa/opencode-loopback.py "$RELAY/paired-demo/"
cp packaging/relay-daemon-wrapper.sh "$RELAY/run-daemon.sh"
chmod -R u+x "$RELAY/mock"/*.sh "$RELAY/mock/catalog"/*.sh "$RELAY/run-daemon.sh"
# macOS：刚拷进 .app 的二进制首次执行时把 stdout 写进包内文件会报 EIO，
# 故先捕获到变量再由 shell 写入。
VERSIONS="$("$RELAY/relayctl" version)"
printf '%s\n' "$VERSIONS" > "$RELAY/version.json"
# 清单用相对名，且按所在目录各存一份：回滚时在各自目录内核对
# （绝对路径在另一台机器上不存在）。
(cd "$RELAY" && shasum -a 256 relayctl) > "$RELAY/SHA256"
(cd "$APP/Contents/MacOS" && shasum -a 256 RelayTaskWindow) > "$APP/Contents/MacOS/SHA256"
NAME="RelayTaskWindow-$VERSION-$BUILD"
rm -f "$OUT/$NAME.zip" "$OUT/$NAME.zip.sha256" "$OUT/$NAME.manifest.json"
(cd "$OUT" && ditto -c -k --keepParent RelayTaskWindow.app "$NAME.zip")
(cd "$OUT" && shasum -a 256 "$NAME.zip" > "$NAME.zip.sha256")
cat > "$OUT/$NAME.manifest.json" <<JSON
{"product": "$VERSION", "build": "$BUILD", "artifact": "$NAME.zip",
 "versions": $VERSIONS, "signed": false, "notarized": false,
 "platform": "macOS arm64 (部署目标 14.0)"}
JSON
echo "packaged $OUT/$NAME.zip"
cat "$OUT/$NAME.zip.sha256"
