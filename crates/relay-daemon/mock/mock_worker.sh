#!/bin/sh
# 项目自带 Mock worker（M1）：只改受管 worktree 内 write_scope 文件。
# 用法：mock_worker.sh <worktree-path>
# 环境变量 MOCK_WORKER_HOLD=1 时挂起等待 TERM（取消/超时回归用）。
set -e
WT="$1"
if [ -z "$WT" ]; then
  echo "usage: mock_worker.sh <worktree>" >&2
  exit 2
fi
case "$2" in
  hold)
    # 挂起等待 TERM（取消/中断回归）；自限时 30s 防泄漏。
    trap 'exit 0' TERM
    i=0; while [ "$i" -lt 30 ]; do sleep 1; i=$((i+1)); done
    exit 0
    ;;
  hold-strict)
    # 忽略 TERM 并派生同组孙进程（进程组残留回归）：升级 SIGKILL 才能回收。
    trap '' TERM
    sleep 300 &
    i=0; while [ "$i" -lt 30 ]; do sleep 1; i=$((i+1)); done
    exit 0
    ;;
  orphan)
    # 父进程立即退出但同组孙进程残留且忽略 TERM（"直接子进程退出后
    # 仍有组成员"）：整组需升级 SIGKILL 才能确认回收。
    (trap '' TERM; sleep 300) &
    exit 0
    ;;
  orphan-no-pipe)
    # 后代关闭输出后继续存活（组仍有成员；退出不可确认）。
    (exec 1>&- 2>/dev/null; sleep 300) &
    exit 0
    ;;
esac
printf '\n// mock edit by relay M1\n' >> "$WT/src/count.rs"
mkdir -p "$WT/tests"
printf '// mock test scaffold\n' >> "$WT/tests/count.rs"
exit 0
