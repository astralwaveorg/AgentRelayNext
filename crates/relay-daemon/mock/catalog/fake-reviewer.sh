#!/bin/sh
# 离线目录项：评审者（只读角色）。包装 mock_reviewer.sh，固定工具与模型身份。
HERE="$(cd "$(dirname "$0")" && pwd)"
exec "$HERE/../mock_reviewer.sh" "$@"
