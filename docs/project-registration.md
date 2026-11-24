# 本机项目登记

一个 daemon 实例当前只接受一个明确登记的本机 Git 主 checkout。使用独立实例目录试用；已安装的演示实例仍保留自己的合成仓库，不会登记其他项目。项目目录不扫描，工作副本仍放在独立受管目录。

在受信任的 daemon 配置中显式指定物理绝对路径 `repo_root`、checkout 之外的 `managed_root`、本机验证规则和：

```json
{
  "project_binding": {
    "project_id": "proj_example",
    "write_scope": ["src/status.txt", "tests/"]
  }
}
```

路径带尾部斜杠表示目录授权；文件授权只覆盖那个文件。客户端的每条路径都必须在登记范围内。登记配置没有添加或审批网络、账户、模型、权限或写入规则。

用 `relayctl --socket <此实例的relay.sock> project` 获取登记的项目 ID、当前执行配置 ID、文件范围和可用验证规则。经本机 socket 读取，MCP 工具不会暴露登记路径。将返回的 ID 和获准的路径、规则传给 `relay_plan`，核对解析出的 Git 基线与范围后再用 `relay_submit` 排队。

执行配置 ID 绑定配置和 Git 根目录及其 `.git` 目录的文件系统身份。更改配置或替换仓库会使旧计划失效；已排队的旧任务在创建受管副本或启动进程前停止。同一个成功请求的幂等重放仍返回首次结果。系统当前不支持在一个实例里切换或编辑多个项目。窗口使用已经登记的实例，暂不写入登记配置。

从连接该实例的任务窗口点「新建任务…」，填写目标、说明、逐行填写写入范围、选择已登记的验证规则。先点「生成计划预览」，核对基线、有效期、路径和规则，再点「确认提交」。若提交响应丢失，窗口内的「重送同一请求」沿用原 request ID；关闭表单前先检查任务列表。未登记的演示实例不会借用其他仓库。

从 0.1.13 起，可通过窗口顶部「登记项目…」选择主 checkout、填写写入范围与验证命令、预览并确认。程序将登记写入 `~/.agent-relay-next/projects/<project-id>/registration.json`。预览不写盘；不执行验证规则；登记不会修改仓库、启用供应商账户或启动服务。该资料可供后续配置独立实例使用，暂不自动安装项目的后台服务。

从 0.1.14 起，在任务窗口顶部「项目」菜单选择已登记的项目，即可查看该项目的独立实例。仍可切回「离线演示」，两边的任务和数据库分开。项目的执行路线未启用时，只允许查看：不会生成执行计划、提交任务、创建工作副本或启动模型和验证命令。切换项目或退出窗口应用会结束由窗口启动的查看服务。

命令行可用 `relayctl project-setup list` 查看登记清单。用 `relayctl project-setup serve --project-id <id>` 启动同一个查看服务；服务输出 socket 路径后保持运行，关闭标准输入时正常退出。项目的 `instance` 和 `managed` 目录在登记目录内。

## 真模型路线预览与本机启用

0.1.15 加入经明确批准后启动的项目执行服务。首次启用使用终端；任务窗口可连接服务并展示每次提交的数据范围。正常打开项目仍是仅查看模式。执行服务不会安装到 LaunchAgent，也不会把账户密钥持久化。项目内每个计划固定 Git 基线，只接受登记范围内 1–16 个已有文本文件，至少选一项本机验证。首个路线只执行一轮，无自动重试、自动返工或模型降级。

先在「项目」菜单退出同一项目的查看服务（切回离线演示），然后预览：

    relayctl project-setup route-preview <项目ID> <claude绝对路径> <opencode绝对路径>

返回的 approval 只适用于这个项目、允许写入的范围、验证规则和两份实际 CLI 二进制。只有确认 MiniMax 可用 M Plan Credits、OpenCode Go 的 Extra Usage 已关闭，并在 shell 中导出两份专用密钥（`MINIMAX_API_KEY`、`OC_API_KEY`，例如写入 `~/.zshrc`）时，才启用。默认实现端点为国际站；中国大陆站账户先导出 `RELAY_MINIMAX_ENDPOINT=https://api.minimax.cn/anthropic/v1/messages`，预览会绑定实际端点：

    set -o pipefail
    {
      printf '%s\n' "$MINIMAX_API_KEY" &&
      printf '%s\n' "$OC_API_KEY"
    } | relayctl project-setup serve-live <项目ID> <claude绝对路径> <opencode绝对路径> \
        --approve <approval> --allow-plan-credits --extra-usage-off

替换非秘密参数，不把密钥放在参数、聊天或日志内。服务输出独立 socket，并在前台运行。可先用 \`relayctl --socket <socket> project\` 查看路线。要用任务窗口连接这个实例，从终端用安装包内的 \`Contents/MacOS/RelayTaskWindow --socket <socket>\` 打开。启动服务和生成计划都不向供应商发送文件；只有任务确认提交后，才按阶段向预览的供应商发送相应数据。验证失败不会发送评审请求。每次重启会产生新执行配置，旧配置下尚未执行的任务不会按新账户直接派发；先检查任务状态再重新预览。

初次真实账户验收仍应优先跑 [合成文件的真实账户样本](route-pilot.md#双工具账户验收同一份改动)，再选择获准向供应商发送的实际项目内容。

从 0.1.17 起，通过供应商身份和响应检查的实现、评审阶段会保存模型与用量事件。用包内 `relayctl --socket <socket> events --after-seq <游标> --limit 100` 查看，根据任务 ID 和 attempt 定位 `route_implement_usage`、`route_review_usage`。MiniMax 未提供可核实的扣费归属，事件标为 `unverified`；供应商未返回的 token 数记为 `null`。OpenCode 事件记录供应商报告的套餐来源、用量及余额扣费。事件不含提示词、代码、补丁正文或原始响应。请求失败且未取得有效响应时，缺少用量事件不能证明没有产生费用。

项目执行服务支持用 Ctrl-C 或 SIGTERM 停机。服务停止接收写请求，回收受管进程，并在退出前清理自己的 socket。无法证实进程已经退出时，保留派发阻断，重启后也不会自动重跑。重启后可查看中断任务，再按任务记录决定下一步。

## 首次双账户验收：只发合成样本

验收脚本 `packaging/qa/live-project-acceptance.py` 直接使用已安装的 relayctl。它走项目登记、计划、提交、受管实现、本机验证、独立评审、制品回读和合成任务验收。它会把补丁应用到一份独立副本比对字节，并检查原合成仓库保持干净。不会向真实工作仓库写入或从中取材料。

预览只在 `/private/tmp` 建立合成仓库，不访问供应商：

```sh
relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
python3 packaging/qa/live-project-acceptance.py prepare   "$relayctl" "$HOME/.local/bin/claude" /opt/homebrew/bin/opencode
```

记录输出中的 `directory` 和 `approval`。只有已核对下列计费选项与预览内容，并已在 shell 中导出 `MINIMAX_API_KEY` / `OC_API_KEY` 后，才运行：

```sh
set -o pipefail
{
  printf '%s\n' "$MINIMAX_API_KEY" &&
  printf '%s\n' "$OC_API_KEY"
} | python3 packaging/qa/live-project-acceptance.py run '<directory>'   --approve '<approval>' --allow-plan-credits --extra-usage-off
```

预览目录只允许执行一次。结果保留在目录内的 `evidence.json`，包含任务 ID、实际模型、用量与可核对的计费字段，以及通过的补丁和收据摘要。失败时先查同一目录里的 `evidence.json`、`service.log` 和供应商账户用量；不要在未确认前重跑。
