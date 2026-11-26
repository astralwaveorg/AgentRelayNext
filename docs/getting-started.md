# 快速上手与运维指南

本指南覆盖安装、本机服务管理、离线演示、MCP 桥接、升级回滚和日常维护。

## 1. 安装与首次启用

1. 从 [GitHub Releases](https://github.com/astralwaveorg/AgentRelayNext/releases/latest) 下载 `RelayTaskWindow-<version>.dmg` 和 `SHA256SUMS`，并核对摘要：

   ```sh
   shasum -a 256 -c SHA256SUMS --ignore-missing
   ```

2. 打开 DMG，将 `RelayTaskWindow.app` 拖入 `~/Applications` 或 `/Applications`。
3. 首次启动未签名预览版时，在 Finder 中按住 Control 点击 App 并选择 **打开**，或在 **系统设置 → 隐私与安全性** 点击“仍要打开”。不要全局关闭 Gatekeeper。
4. 启动窗口后点击 **启用本地预览**，或在终端运行：

   ```sh
   relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
   "$relayctl" local-preview enable
   "$relayctl" local-preview status
   ```

### 本机服务管理命令

| 命令 | 作用 |
| --- | --- |
| `relayctl local-preview enable` | 初始化 `~/.agent-relay-next/`，安装稳定包装脚本 `run-daemon.sh`，注册当前用户 LaunchAgent，并运行 `doctor` 和 `mcp-verify` |
| `relayctl local-preview status` | 查看服务状态（`not_configured`、`configured_not_running`、`starting`、`running` 或 `unhealthy`） |
| `relayctl local-preview disable` | 停用并卸载 Relay 自己的 LaunchAgent，默认保留数据 |

### 本机目录布局

- 应用包位于 `~/Applications/RelayTaskWindow.app`。
- 实例与数据位于 `~/.agent-relay-next/`。运行期目录权限为 `0700`，socket 和 `.own` 权限为 `0600`。
- LaunchAgent 标签为 `net.relay.candidate.local-preview`，只对当前用户生效。关闭窗口后后台服务继续运行。
- 本机 Unix socket 路径为 `~/.agent-relay-next/instance/relay.sock`。

## 2. 离线演示与验收体验

打开 **RelayTaskWindow** 后按以下步骤体验完整流程：

1. 点击 **运行离线演示**。系统在临时合成仓库中依次完成实现、本地验证、独立只读评审和一轮自动返工，并生成制品。
2. 等任务变为 **待验收（`awaiting_review`）** 后，点击 **加载差异 / 测试收据 / 交付说明** 查看 SHA-256 绑定的完整证据。
3. 点击 **验收交付…** 选择接受或拒绝。弹窗打开时会锁定当前的 `attempt`、`revision`、补丁和收据摘要。验收只记录最终结论，不会改动主分支。
4. 如果本机安装了 Claude Code 和 OpenCode CLI，也可以点击 **运行双工具样本**。该入口用真实 CLI 加本机合成响应测试三阶段流程，不发外部请求，也不消耗额度。

## 3. MCP 接入与 CLI 诊断

启动 MCP stdio 桥：

```sh
"$relayctl" mcp --socket "$HOME/.agent-relay-next/instance/relay.sock"
```

验证 MCP 握手和工具清单：

```sh
"$relayctl" mcp-verify --socket "$HOME/.agent-relay-next/instance/relay.sock"
```

运行只读诊断 `doctor`。该命令以只读模式打开 SQLite，并核查 schema 表结构、socket、派发门和受管进程组状态：

```sh
"$relayctl" doctor --instance-dir "$HOME/.agent-relay-next/instance"
```

在命令行导出与窗口一致的文本摘要：

```sh
"$HOME/Applications/RelayTaskWindow.app/Contents/MacOS/RelayTaskWindow" \
  --socket "$HOME/.agent-relay-next/instance/relay.sock" --dump
```

## 4. 升级、回滚与只读档案恢复

### 跨版本升级

升级前先确认没有排队或运行中的任务。跨版本升级要求产品版本不同、业务协议与 wire 一致：

```sh
packaging/upgrade-local-preview.sh <新版-RelayTaskWindow.zip>
```

脚本按顺序执行以下动作：先检查空闲，停服后二次复查 SQLite 活跃任务数，写入维护标记，在 `~/Applications/.relay-backup.*` 生成快照和 `metadata.json`，替换应用与包装脚本，最后启动新版并通过 MCP 健康检查和 `doctor`。如果新版启动失败，脚本自动恢复旧版并保留现场。

### 回滚

不要在服务运行期间手工覆盖 `.app`。请使用回滚脚本：

```sh
packaging/rollback.sh "$HOME/Applications" [备份目录或后缀]
```

回滚脚本先停服并为当前状态另建一份备份，核对 `app_sha256` 和 `database_snapshot_sha256` 后还原同 schema 快照。从 v9 降级到 v8 时走内置的 `maintenance rollback-v8`，其他跨 schema 降级会直接拒绝。

### 只读档案恢复

你可以把历史备份恢复成只读档案进行审计查看，不影响在线实例：

```sh
"$relayctl" maintenance restore <备份目录/data> /private/tmp/relay-archive
"$relayctl" maintenance serve-archive --instance-dir /private/tmp/relay-archive/instance
```

档案模式禁止提交、取消和验收。你可以用 `relayctl status`、`relayctl artifact` 或 `RelayTaskWindow --socket <archive.sock>` 查看历史补丁和收据。

## 5. 配额、清理与日志轮转

- 单实例最多容纳 20 个非终态任务。重复提交同一 `request_id` 的幂等重放仍会正常返回。
- 受管制品和工作树总配额为 2 GiB。磁盘可用空间低于 512 MiB 时暂停领取新任务，正在运行的任务允许完成并保存证据。
- 日志保留最近 5 份，单份上限 5 MiB。`run-daemon.sh` 在启动和运行期间负责轮转，停服后也可以运行 `maintenance rotate-logs` 手动轮转。
- 运行 `prune` 前必须先停止服务。命令默认只输出预览清单，加上 `--apply` 才会删除超期终态工作树、制品、临时 `review/`、`.claude-implement-*`、`.staging-*` 目录以及超期审计元数据：

```sh
# 1. 预览待清理项
"$relayctl" maintenance prune --instance-dir "$HOME/.agent-relay-next/instance" --older-than-days 30

# 2. 确认清单后再执行（--metadata-days 默认 90）
"$relayctl" maintenance prune --instance-dir "$HOME/.agent-relay-next/instance" \
  --older-than-days 30 --metadata-days 90 --apply

# 手动轮转日志（停服后）
"$relayctl" maintenance rotate-logs --instance-dir "$HOME/.agent-relay-next/instance"
```

## 6. 卸载

先停用本机服务：

```sh
"$relayctl" local-preview disable
```

然后把 `RelayTaskWindow.app` 移入废纸篓。`disable` 默认保留 `~/.agent-relay-next/`。如果要彻底清除历史任务和日志，请确认后再手动删除该目录。
