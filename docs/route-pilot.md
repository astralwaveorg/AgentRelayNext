# MiniMax M Plan 路线资格核验

本入口分别核验两个候选：Claude Code 实现路线使用 `MiniMax-M3.1-Flash`，OpenCode 评审路线使用 `MiniMax-M3`。它不会启用 daemon 的真实路线，也不接收项目路径或任务内容。每次只向 MiniMax 的 Anthropic Messages HTTPS 入口发送一条内置样本，最大输出 512 tokens。两个预览各有自己的批准指纹。CLI 只连本机一次性网关，真实密钥不进入 CLI 环境、不作为参数、不落盘。请求不跟随重定向，不自动重试，不使用备用账户。

先用安装包内命令预览实际内容，或用合成响应核验本机 CLI：

```sh
relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
claude="$HOME/.local/bin/claude"
"$relayctl" route-pilot preview "$claude"
"$relayctl" route-pilot offline-check "$claude"
```

OpenCode 入口从 0.1.5 起随本机安装包提供：

```sh
opencode=/opt/homebrew/bin/opencode
"$relayctl" route-pilot review-preview "$opencode"
"$relayctl" route-pilot review-offline-check "$opencode"
```

`preview` 的 approval 绑定预览内容和实际 CLI 文件摘要。只有在账户所有者确认 M Plan 专用密钥、额度和这次样本后才可真实调用。根据 [MiniMax M Plan 说明](https://platform.minimax.io/docs/m-plan/quickstart.md)，套餐不足或模型未包含时可能扣 Credits；如不接受，停在离线步骤。本命令无法在请求前证明最终归属。如账户所有者已经通过钥匙串访问界面用指定名称单独保存专用密钥，可用下列方式调用。在命令里只替换非秘密的 `<approval>`，不要将密钥贴到命令、聊天或日志里：

```sh
set -o pipefail
/usr/bin/security find-generic-password -w -s 'AgentRelay MiniMax M Plan pilot' |
  "$relayctl" route-pilot run "$claude" --approve '<approval>' --allow-plan-credits
```

HTTP 认证、限流、请求不匹配、实际模型不符或缺终态都停止。若请求已发送后失败，先查原生账单再决定是否重试。通过时输出工具、角色、已核对模型和供应商流中提供的 token 数，实际扣减仍标 `unverified`，须与原生记录核对。

评审样本使用同样的标准输入密钥方式，子命令改为 `route-pilot review-run "$opencode" --approve '<review-approval>' --allow-plan-credits`。从 `review-preview` 复制该指纹，不能复用 Claude Code 的批准。先确认原生账户对 `MiniMax-M3` 的实际使用资格和扣减规则；供应商的 [Anthropic API 模型列表](https://platform.minimax.io/docs/api-reference/text-anthropic-api.md) 以及 [M Plan FAQ](https://platform.minimax.io/docs/m-plan/faq.md) 不能证明单个账户的权益。两个样本都通过只表示工具能按预览到达各自模型并正常完成；还需在实际账户核对扣减，并用获准的工作样本验证真实实现、评审和最终人工放行。

## OpenCode Go：GLM-5.3-Flash

`go-preview` 与 `go-offline-check` 核验 `opencode-go/glm-5.3-flash`。这条路线使用 OpenCode Go 官方模型目录列出的 Chat Completions 接口，不使用上文的 MiniMax 评审候选。在仓库开发版中可这样重跑：

```sh
cargo run -q -p relay-daemon --locked --offline --bin relayctl -- route-pilot go-preview /opt/homebrew/bin/opencode
cargo run -q -p relay-daemon --locked --offline --bin relayctl -- route-pilot go-offline-check /opt/homebrew/bin/opencode
```

账户所有者可在 OpenCode Console 的 Go 页面确认 **Extra Usage 已关闭**。已经将该订阅工作区的 Console API key 单独保存到钥匙串时，可先查询三档额度；该步骤不发模型请求。真实样本只发送内置评审小题，最多一次请求、512 个输出 tokens、不开启工具。只有实际模型、正常终态、样本结果、套餐计费来源和零余额扣费都能验证时才通过。

```sh
set -o pipefail
/usr/bin/security find-generic-password -w -s 'AgentRelay OpenCode Go pilot' |
  cargo run -q -p relay-daemon --locked --offline --bin relayctl -- route-pilot go-quota /opt/homebrew/bin/opencode

/usr/bin/security find-generic-password -w -s 'AgentRelay OpenCode Go pilot' |
  cargo run -q -p relay-daemon --locked --offline --bin relayctl -- route-pilot go-run /opt/homebrew/bin/opencode --approve '<go-approval>' --extra-usage-off
```

`--extra-usage-off` 表示操作者已经在 Go 页面核对该开关；额度接口不回传开关，额度查询也无法为随后请求预留容量。回执中的额度用量与余额扣费取自真实供应商响应，离线合成回执不作账单凭证。不会读取 OpenCode 的本机授权文件、修改全局模型、自动重试、切换账户或启用 daemon 的真实路由。若真实请求报错且不能确定是否送达，先核对 Go 原生用量再决定是否重跑。

## Claude Code：受限文件修改验收

`edit-preview` 预览固定的 `status.txt` 样本，只要求把 `status=waiting` 改成 `status=ready` 并保留换行。不会打开你的项目。离线检查使用真实 Claude Code CLI、合成供应商响应、临时 Git 基线和项目自己的受管 worktree 与写入校验器。验证会比较最终文件字节、确认只有该文件发生变化，并检查基线仍干净。结果只输出模型、差异摘要和验证收据摘要。

```sh
relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
claude="$HOME/.local/bin/claude"
"$relayctl" route-pilot edit-preview "$claude"
"$relayctl" route-pilot edit-offline-check "$claude"
```

真实验证须使用本命令自己的 approval，并明确接受其使用 M Plan Credits 的可能。只有接受该条件时才附 `--allow-plan-credits`：

```sh
set -o pipefail
/usr/bin/security find-generic-password -w -s 'AgentRelay MiniMax M Plan pilot' |
  "$relayctl" route-pilot edit-run "$claude" --approve '<edit-approval>' --allow-plan-credits
```

验收样本最多向已固定的 HTTPS 入口请求一次，拒绝路径、原始摘要或响应模型不匹配；不运行模型返回的内容，不自动写回主 checkout。一次样本通过仍不代表真实双工具任务、长期账户额度或常驻派发已通过。

## 双工具账户验收：同一份改动

`pair-offline-check` 依次使用本机 Claude Code 与 OpenCode CLI：修改固定合成文件、验证字节、把实际补丁和收据交给评审，最后同时核对两份摘要。两个供应商响应均为合成。可用 `--stale-diff` 或 `--stale-receipt` 验证错误摘要会分别被拒绝。

```sh
relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
claude="$HOME/.local/bin/claude"
opencode=/opt/homebrew/bin/opencode
"$relayctl" route-pilot pair-preview "$claude" "$opencode"
"$relayctl" route-pilot pair-offline-check "$claude" "$opencode"
```

真实模式需要两种账户条件都满足：已接受 MiniMax M Plan 样本可能使用 Credits，且已在 OpenCode Go 控制台确认 Extra Usage 关闭。已将对应密钥按本文指定名称存入钥匙串时，可以用同一个命令顺序完成两段验收，不回传密钥或项目资料：

```sh
set -o pipefail
{
  /usr/bin/security find-generic-password -w -s 'AgentRelay MiniMax M Plan pilot' &&
  /usr/bin/security find-generic-password -w -s 'AgentRelay OpenCode Go pilot'
} | "$relayctl" route-pilot pair-run "$claude" "$opencode" \
  --approve '<pair-approval>' --allow-plan-credits --extra-usage-off
```

两个 CLI 的版本文件摘要、合成样本、评审数据范围和计费确认绑定在一次批准里；不复用单路批准。只发送一个合成实现和一个只读评审请求，不自动重试或返工。Go 额度在实现前与发评审请求前各查一次。实现阶段如果没有通过本机验证，不发送评审请求。若评审没有通过，本次不交付候选。此模式仍是资格验收，既不提交常驻 daemon 任务，也不打开真实项目派发。

评审只接收任务目标、当前补丁、验证规则 ID 和通过状态，以及本机完整收据和补丁的摘要。完整收据仍留在本机；验证程序参数和输出不发给模型。供应商密钥和请求正文均通过私有标准输入交给 HTTPS 客户端，不出现在进程参数中。
