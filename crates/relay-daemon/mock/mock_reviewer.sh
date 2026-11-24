#!/bin/sh
# M2.5 离线评审夹具（只读角色；与 mock_adapter.sh 同一接口：probe | run）。
# 评审输入只有白名单 diff 与测试收据（不接触 worktree），输出一帧
# ReviewReport v1。行为由环境变量选择（daemon 经按角色的子进程 env 传入）：
#   REVIEWER_ROLES / REVIEWER_CAPABILITIES / REVIEWER_MODEL   probe 上报内容
#   REVIEW_MODE  strict(默认：diff 含 TODO 即要求修改) | always-reject |
#                stale-digest | wrong-receipt | rogue-write | inconclusive |
#                blocking-pass | reject-no-finding | too-many-findings |
#                oversized | bad-schema | not-success | no-verdict | dup-verdict
#   REVIEW_ROGUE_TARGET   rogue-write 模式下尝试写入的文件（验证评审不能写）
MODE="$1"; shift
emit() { printf '%s\n' "$1"; }
ROLES="${REVIEWER_ROLES-review}"
CAPS="${REVIEWER_CAPABILITIES-read_files}"
MODEL="${REVIEWER_MODEL-fake-reviewer-v1}"
json_list() {
  [ -z "$1" ] && return 0
  printf '%s\n' "$1" | tr ',' '\n' | sed 's/.*/"&"/' | paste -sd, -
}
finding() {
  # severity path location evidence suggestion
  printf '{"path":"%s","location":"%s","severity":"%s","evidence":"%s","suggestion":"%s"}' \
    "$2" "$3" "$1" "$4" "$5"
}
report() {
  # verdict findings_json diff_sha receipt_sha schema
  printf '{"type":"review_verdict","report":{"schema_version":%s,"candidate_diff_sha256":"%s","test_receipt_sha256":"%s","verdict":"%s","findings":[%s]}}' \
    "$5" "$3" "$4" "$1" "$2"
}
sha256_file() { shasum -a 256 "$1" | cut -d' ' -f1; }
case "$MODE" in
  probe)
    # 模型身份为空 = 不上报身份（未核实）。
    MODEL_FIELD="\"actual_model\":\"$MODEL\","
    [ -z "$MODEL" ] && MODEL_FIELD=""
    emit "{\"type\":\"probe\",\"binary\":\"mock-reviewer\",\"version\":\"1.0\",\"roles\":[$(json_list "$ROLES")],$MODEL_FIELD\"capabilities\":[$(json_list "$CAPS")]}"
    exit 0
    ;;
  run)
    DIR="$1"
    emit '{"type":"worker_started","attempt_ref":"mock-review"}'
    DIFF="$DIR/diff.patch"
    RECEIPT="$DIR/test_receipt.json"
    DIFF_SHA=$(sha256_file "$DIFF")
    RECEIPT_SHA=$(sha256_file "$RECEIPT")
    TODO_FINDING=$(finding major src/count.rs "end of file" "diff leaves a TODO placeholder from round 1" "replace the placeholder with the real implementation")
    case "${REVIEW_MODE:-strict}" in
      strict)
        if grep -q 'TODO' "$DIFF"; then
          emit "$(report changes_requested "$TODO_FINDING" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        else
          emit "$(report pass "" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        fi
        ;;
      always-reject)
        emit "$(report changes_requested "$TODO_FINDING" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        ;;
      stale-digest)
        # 用旧补丁的摘要放行：摘要绑定不符，不得放行。
        emit "$(report pass "" "0000000000000000000000000000000000000000000000000000000000000000" "$RECEIPT_SHA" 1)"
        ;;
      wrong-receipt)
        emit "$(report pass "" "$DIFF_SHA" "0000000000000000000000000000000000000000000000000000000000000000" 1)"
        ;;
      rogue-write|rogue-write-reject)
        # 评审不应写受审候选：尝试写入（daemon 事后比对候选变化）。
        printf '\n// written by reviewer (must be detected)\n' >> "${REVIEW_ROGUE_TARGET:?rogue-write needs REVIEW_ROGUE_TARGET}"
        if [ "${REVIEW_MODE:-}" = "rogue-write-reject" ]; then
          emit "$(report changes_requested "$TODO_FINDING" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        else
          emit "$(report pass "" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        fi
        ;;
      inconclusive)
        emit "$(report inconclusive "" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        ;;
      blocking-pass)
        emit "$(report pass "$(finding blocking src/count.rs "end of file" "a blocking concern is recorded alongside pass" "resolve before acceptance")" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        ;;
      reject-no-finding)
        emit "$(report changes_requested "" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        ;;
      too-many-findings)
        many="" i=0
        while [ "$i" -lt 25 ]; do
          many="${many}$(finding minor src/count.rs "line $i" "finding $i" "suggestion $i"),"
          i=$((i+1))
        done
        emit "$(report changes_requested "${many%,}" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        ;;
      oversized)
        big=$(printf 'x%.0s' $(seq 1 20000))
        emit "$(report pass "$(finding info src/count.rs "n/a" "$big" "none")" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        ;;
      bad-schema)
        emit "$(report pass "" "$DIFF_SHA" "$RECEIPT_SHA" 2)"
        ;;
      no-verdict)
        ;;
      dup-verdict)
        emit "$(report pass "" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        emit "$(report changes_requested "$TODO_FINDING" "$DIFF_SHA" "$RECEIPT_SHA" 1)"
        ;;
      not-success)
        emit '{"type":"worker_finished","termination":"auth_failed"}'
        exit 7
        ;;
    esac
    emit '{"type":"worker_finished","termination":"success"}'
    exit 0
    ;;
  *)
    emit "{\"type\":\"error\",\"detail\":\"unknown mode $MODE\"}"
    exit 2
    ;;
esac
