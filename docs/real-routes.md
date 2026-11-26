# 真实路线与项目登记

本文说明如何在本机 Git 仓库中登记项目、预览并启动真实双模型路线，以及模型目录、重试、备用模型和隐私计费边界。

> **当前版本范围（`0.1.x`）。** 默认真实路线固定为 Claude Code（MiniMax）实现、本地命令验证、OpenCode Go（DeepSeek）只读评审。你可以通过环境变量覆盖模型和同账户备用模型。开放 BYOK 和多 CLI 组合计划在 [后续开发方案](roadmap.md) 的 `v0.2` 阶段实现。

## 1. 登记本机 Git 项目

每个登记项目拥有独立的实例目录和受管目录，位于 `~/.agent-relay-next/projects/<project-id>/`，与离线演示实例隔离。

### 在任务窗口登记

1. 点击顶部 **登记项目…**，选择目标 Git 仓库的物理根目录。
2. 填写允许写入的路径白名单（`write_scope`）。文件路径按精确路径匹配，目录路径须带尾部 `/`。
3. 填写固定本地验证命令，包括可执行文件绝对路径、固定 `argv` 和超时时间。命令不经过 shell。
4. 预览并保存。登记只写入 `registration.json`，不会改动仓库，不会运行验证命令，也不会调用外部模型。

### 用命令行查看或启动只读服务

```sh
relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
"$relayctl" project-setup list
"$relayctl" project-setup serve --project-id <project-id>
```

只读模式可以浏览历史任务和制品，但不会创建 worktree，也不会派发外部任务。

## 2. 预览与启动真实执行路线

真实执行服务在前台运行，不注册到 LaunchAgent，也不持久化 API Key。

### 第一步：生成路线预览与批准指纹

先退出同一项目的只读查看服务，再执行预览：

```sh
"$relayctl" project-setup route-preview <project-id> <claude绝对路径> <opencode绝对路径>
```

返回的 `approval` 是 SHA-256 指纹，绑定项目配置、写入范围、验证规则、两个 CLI 二进制摘要、实际端点、主备模型和重试参数。其中任何一项变动都需要重新预览。

### 第二步：按需设置环境变量

在运行 `route-preview` 和 `serve-live` 前，可以在当前 shell 中导出以下变量：

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `RELAY_MINIMAX_ENDPOINT` | `https://api.minimax.io/anthropic/v1/messages` | 可切换中国大陆站 `https://api.minimax.cn/anthropic/v1/messages`，非法值直接拒绝 |
| `RELAY_IMPLEMENT_MODEL` | `MiniMax-M3.1-Flash` | 覆盖实现模型，归一化后不能与评审模型相同 |
| `RELAY_IMPLEMENT_FALLBACK_MODEL` | 未设置 | 实现阶段遇额度类错误时的同账户备用模型 |
| `RELAY_GO_MODEL` | `deepseek-v4.1-flash` | 覆盖 OpenCode Go 评审模型，需在对应工作区已启用 |
| `RELAY_REVIEW_FALLBACK_MODEL` | 未设置 | OpenCode Go 额度耗尽时的同账户备用模型 |
| `RELAY_RETRY_MAX` | `10`（上限 `20`） | 上游 429 或瞬时传输失败的最大重试次数 |
| `RELAY_RETRY_INTERVAL_MS` | `30000`（`1000` 到 `120000`） | 重试间隔，单位毫秒 |

### 第三步：通过私有 stdin 传入密钥并启动

确认账户额度与计费设置并在 shell 中导出 `MINIMAX_API_KEY` 和 `OC_API_KEY` 后，运行以下命令启动服务：

```sh
set -o pipefail
{
  printf '%s\n' "$MINIMAX_API_KEY" &&
  printf '%s\n' "$OC_API_KEY"
} | "$relayctl" project-setup serve-live <project-id> <claude绝对路径> <opencode绝对路径> \
      --approve '<approval>' --allow-plan-credits --extra-usage-off
```

启动成功后，stdout 会输出包含 `socket` 路径的 JSON。你可以用任务窗口连接这个实例：

```sh
"$HOME/Applications/RelayTaskWindow.app/Contents/MacOS/RelayTaskWindow" --socket <socket>
```

每次重启 `serve-live` 都会生成新的执行配置 ID。旧配置下尚未执行的任务不会在新会话中自动派发。按 `Ctrl-C` 或发送 `SIGTERM` 会优雅停机，回收受管进程组并清理 socket。

## 3. 模型目录与独立性规则

查看内置只读目录和推荐组合：

```sh
"$relayctl" catalog list
"$relayctl" catalog recommend
```

| 目录条目 | 工具 | 角色 | 能力 | 状态 |
| --- | --- | --- | --- | --- |
| `minimax/MiniMax-M3.1-Flash` | Claude Code | `implement` | `read_files`, `edit_files` | 预览（`preview_only: true`） |
| `opencode-go/deepseek-v4.1-flash` | OpenCode Go | `review` | `read_files` | 预览（`preview_only: true`） |
| `offline/fake-implementer-v1` | 本地 fake | `implement` | `read_files`, `edit_files` | 仅离线演示 |
| `offline/fake-reviewer-v1` | 本地 fake | `review` | `read_files` | 仅离线演示 |

独立性检查强制执行三条规则：

1. 实现者和评审者必须使用不同的可执行文件和工具标识。
2. 双方模型经过统一的 `normalize_model()`（小写、去空白、去 provider 前缀、展开别名）后必须不同。别名不构成独立评审。
3. 评审者只声明 `read_files`。如果实现失败或本地验证未通过，系统不会发起评审请求。

## 4. 隔离验收与路线资格预检

### 合成仓库端到端验收

首次测试真实账户时，建议先用 `packaging/qa/live-project-acceptance.py` 在 `/private/tmp` 跑一次性合成仓库，不触碰真实业务代码：

```sh
# 1. 本地预览，不联网，不消耗额度
python3 packaging/qa/live-project-acceptance.py prepare \
  "$relayctl" "$HOME/.local/bin/claude" /opt/homebrew/bin/opencode

# 2. 核对输出的 directory 和 approval 后运行一次
set -o pipefail
{
  printf '%s\n' "$MINIMAX_API_KEY" &&
  printf '%s\n' "$OC_API_KEY"
} | python3 packaging/qa/live-project-acceptance.py run '<directory>' \
      --approve '<approval>' --allow-plan-credits --extra-usage-off
```

运行完成后，可以导出可公开审阅的脱敏证据包：

```sh
python3 packaging/qa/evidence-bundle.py <sample-dir> docs/evidence/<name>.json
```

已归档的真实双账户脱敏记录见 [docs/evidence/README.md](evidence/README.md)。

### 单路资格预检（`route-pilot`）

在接入项目前，可以单独验证 CLI 连通性与额度水位：

```sh
# 离线检查，不发外部请求
"$relayctl" route-pilot offline-check "$HOME/.local/bin/claude"
"$relayctl" route-pilot go-offline-check /opt/homebrew/bin/opencode
"$relayctl" route-pilot pair-offline-check "$HOME/.local/bin/claude" /opt/homebrew/bin/opencode

# 查询 OpenCode Go 额度，不发模型请求
printf '%s\n' "$OC_API_KEY" | "$relayctl" route-pilot go-quota /opt/homebrew/bin/opencode
```

## 5. 数据、隐私与计费边界

- 实现阶段只发送任务目标、说明、白名单内的既有文本文件和基线哈希。单阶段输入上限为 64 KiB，最多包含 16 个文件。
- 评审阶段只发送任务目标、当前补丁、测试通过状态、规则 ID 和双摘要。完整工作区、终端输出和用户目录不会发给评审模型。
- 密钥只经私有管道进入一次性 worker 进程。落盘事件和错误原因会经过凭据过滤并截断到 256 字节以内。
- 通过校验的实现和评审阶段分别记录 `route_implement_usage` 与 `route_review_usage` 事件。MiniMax 扣费归属标为 `unverified`。OpenCode Go 强制检查供应商返回的套餐来源（`go` 或 `go-plus`）且 `balance_charge_usd == 0`。
- 重试预算耗尽或遇到认证、配额异常时任务直接失败，不生成制品，也不自动切换账户。
