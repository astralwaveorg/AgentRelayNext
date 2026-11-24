#!/bin/sh
# M2 离线适配器夹具（05 事件收敛面）：mode probe | run。
# 固定模拟输入，不发网络请求。行为全部由环境变量选择（daemon 经按角色
# 的子进程 env 传入，不改 daemon 自身环境）：
#   ADAPTER_ROLES          probe 上报的角色，逗号分隔（默认 implement）
#   ADAPTER_IDENTITY       probe 上报的工具标识（默认 mock-adapter）
#   ADAPTER_ACTUAL_MODEL   probe 上报的实际模型；置空 = 不上报身份
#   ADAPTER_CAPABILITIES   逗号分隔能力清单
#   ADAPTER_EMIT           run 的协议行为：success|auth_failed|rate_limited|
#                          unsupported_protocol|unknown_event|invalid_json|
#                          huge_frame|hold_stdout|alive_no_pipe|hang|
#                          duplicate_terminal|conflicting_terminal
#   ADAPTER_HOLD_SECONDS   hold/hang 类模式的持续秒数
#   ADAPTER_PROBE_HANG=1   probe 挂起不退出
#   IMPLEMENT_MODE         run 的写入行为：default | rework-once
#   IMPLEMENT_ROUND        当前实现轮次（1 起；rework-once 据此留缺陷或修复）
MODE="$1"; shift
emit() { printf '%s\n' "$1"; }
ROLES="${ADAPTER_ROLES-implement}"
IDENTITY="${ADAPTER_IDENTITY-mock-adapter}"
MODEL="${ADAPTER_ACTUAL_MODEL-fake-adapter-v1}"
CAPS="${ADAPTER_CAPABILITIES-read_files,edit_files}"
json_list() {
  # 逗号分隔 → JSON 字符串数组
  [ -z "$1" ] && return 0
  printf '%s\n' "$1" | tr ',' '\n' | sed 's/.*/"&"/' | paste -sd, -
}
case "$MODE" in
  probe)
    # probe 挂起：等待上限触发（不退出）。
    if [ "${ADAPTER_PROBE_HANG:-0}" = "1" ]; then
      sleep "${ADAPTER_HOLD_SECONDS:-30}"
      exit 0
    fi
    IDENTITY_FIELD="\"actual_model\":\"$MODEL\","
    [ -z "$MODEL" ] && IDENTITY_FIELD=""
    emit "{\"type\":\"probe\",\"binary\":\"$IDENTITY\",\"version\":\"1.0\",\"roles\":[$(json_list "$ROLES")],$IDENTITY_FIELD\"capabilities\":[$(json_list "$CAPS")]}"
    exit 0
    ;;
  run)
    WT="$1"
    emit '{"type":"worker_started","attempt_ref":"mock"}'
    case "${ADAPTER_EMIT:-success}" in
      unknown_event)
        emit '{"type":"totally_unknown","junk":1}'
        ;;
      invalid_json)
        emit 'not-json-at-all'
        ;;
      huge_frame)
        # 单帧超限（超过帧上限）。
        i=0
        while [ "$i" -lt 2000 ]; do
          printf 'x%.0s' $(seq 1 64)
          i=$((i+1))
        done
        printf '\n'
        ;;
      hold_stdout)
        # 后代进程持续持有 stdout 管道：父进程退出后管道仍被持有。
        ( sleep "${ADAPTER_HOLD_SECONDS:-3}" ) &
        exit 0
        ;;
      alive_no_pipe)
        # 后代关闭 stdout 后继续存活（组仍有成员 → 整组退出不可确认）。
        ( exec 1>&- 2>/dev/null; sleep "${ADAPTER_HOLD_SECONDS:-3}" ) &
        exit 0
        ;;
      hang)
        # 不退出（等待上限触发）。
        sleep "${ADAPTER_HOLD_SECONDS:-30}"
        exit 0
        ;;
      duplicate_terminal)
        # 重复终止帧：发出后继续正常写入，验证协议层拒绝。
        emit '{"type":"worker_finished","termination":"success"}'
        emit '{"type":"worker_finished","termination":"success"}'
        ;;
      conflicting_terminal)
        # 冲突终止帧：首帧结果不被后一帧覆盖。
        emit '{"type":"worker_finished","termination":"success"}'
        emit '{"type":"worker_finished","termination":"auth_failed"}'
        ;;
    esac
    case "${IMPLEMENT_MODE:-default}" in
      rework-once)
        if [ "${IMPLEMENT_ROUND:-1}" = "1" ]; then
          # 首轮：留一个可被评审发现的占位缺陷。
          printf '\n// TODO: placeholder left by round 1\n' >> "$WT/src/count.rs"
        else
          # 返工：移除占位，补上修复。
          grep -v 'TODO: placeholder left by round 1' "$WT/src/count.rs" > "$WT/src/count.rs.rework" \
            && mv "$WT/src/count.rs.rework" "$WT/src/count.rs"
          printf '\n// fixed in rework round %s\n' "${IMPLEMENT_ROUND}" >> "$WT/src/count.rs"
        fi
        ;;
      *)
        printf '\n// adapter edit (mock M2 chain)\n' >> "$WT/src/count.rs"
        ;;
    esac
    case "${ADAPTER_EMIT:-success}" in
      auth_failed)
        emit '{"type":"worker_finished","termination":"auth_failed"}'
        exit 7
        ;;
      rate_limited)
        emit '{"type":"worker_finished","termination":"rate_limited","diagnostic":"GO_QUOTA_EXHAUSTED"}'
        exit 8
        ;;
      unsupported_protocol)
        emit '{"type":"worker_finished","termination":"unsupported_protocol"}'
        exit 9
        ;;
      *)
        emit '{"type":"worker_finished","termination":"success"}'
        exit 0
        ;;
    esac
    ;;
  *)
    emit "{\"type\":\"error\",\"detail\":\"unknown mode $MODE\"}"
    exit 2
    ;;
esac
