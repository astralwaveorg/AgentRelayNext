#!/bin/sh
# 离线目录项：实现者（可写角色）。包装 mock_adapter.sh，固定工具与模型身份。
HERE="$(cd "$(dirname "$0")" && pwd)"
ADAPTER_IDENTITY="${ADAPTER_IDENTITY-fake-implementer}" \
ADAPTER_ROLES="${ADAPTER_ROLES-implement}" \
ADAPTER_ACTUAL_MODEL="${ADAPTER_ACTUAL_MODEL-fake-implementer-v1}" \
exec "$HERE/../mock_adapter.sh" "$@"
