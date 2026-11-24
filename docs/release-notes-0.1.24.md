# Agent Relay 0.1.24 — 本机开发预览

**One coding agent writes. Another reviews. You get the evidence.**

这是 Agent Relay 的本机开发预览，不是签名/公证的正式 macOS 发行版。DMG 未签名，
首次启动需要手动放行；不要关闭 Gatekeeper。

## 下载

- `RelayTaskWindow-0.1.24.dmg`
- 校验：下载同目录的 `RelayTaskWindow-0.1.24.dmg.sha256`，或使用 Release 附件中的
  `SHA256SUMS`；构建号见 `.dmg.manifest.json` 和 `RelayTaskWindow-*.zip` 文件名。
- 构建：product `0.1.24`，业务 `relay.v1`，wire `1`，schema `9`

## 本版内容

- 20 个非终态任务队列上限，幂等重放仍可用。
- 受管制品/工作树 2 GiB 配额，超限暂停新领取。
- `maintenance prune`：默认预览，支持 30 天制品/工作树清理和 90 天元数据清理。
- 启动包装脚本在服务启动/重启时自动按 5 MiB × 5 轮转日志；另有手动
  `maintenance rotate-logs`。
- 版本化模型目录和确定性角色推荐（`relayctl catalog list|recommend`）。
- 任务详情 StageRun 时间线；更现代的窗口/菜单栏外观和可访问性标签。
- MIT License。
- 基线检查只读取仓库本地 Git 配置，不再被全局 Git LFS filter 误拦。

## 安装

1. 核对下载摘要：

   ```sh
   shasum -a 256 RelayTaskWindow-0.1.24.dmg
   ```

2. 打开 DMG，把 `RelayTaskWindow.app` 拖到 `Applications`。
3. 首次启动若被 macOS 阻止：Control-click App → 打开，或到“系统设置 → 隐私与安全性”
   选择“仍要打开”。这是未签名/未公证包的预期行为。
4. 用包内 CLI 检查：

   ```sh
   relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
   "$relayctl" doctor --instance-dir "$HOME/.agent-relay-next/instance"
   ```

5. 离线演示不调用真实模型；真实路线需要安装 Git、Claude Code/OpenCode，并由账户
   所有者确认套餐与专用凭据。

## 验证摘要

- `./scripts/check full`：233 passed / 0 failed / 0 ignored，30 suites。
- doctor、MCP 8 项工具、真实 Codex 0.169.0 发现、离线双工具/单路合成链、账本/档案
  恢复通过。
- DMG 挂载、App/relayctl 存在性和版本校验通过；没有 Developer ID/公证/Gatekeeper
  正例。

## 已知限制与未完成门

- **真实双账户端到端未运行**：`AgentRelay MiniMax M Plan pilot` 和
  `AgentRelay OpenCode Go pilot` 两条钥匙串查询仍返回 44；没有消耗真实额度或发送真实
  供应商请求。不要把合成端点结果当成真实套餐证据。
- 无 Apple 开发者账号：没有 Developer ID、notarization、stapled ticket 或 Gatekeeper
  通过声明；只提供未签名 DMG 开发预览。
- 干净 macOS 用户、第二 UID、受管断电、菜单栏/可访问性运行期验收未完成。
- 性能 P95 ≤100 ms 目标未在空闲参考机证明；当前本机高内存压力测量约 169 ms。
- 自动日志轮转只在服务启动/重启时执行；持续运行期间仍需维护命令或重启。
- 模型目录只含已验证的本机路线，不是全市场型号目录；目录升级/别名重定向/型号退役
  和预授权备用路线仍未实现。
- 工作副本不是操作系统沙箱；只服务可信本机仓库和可信本机用户。

构建与安装见 `docs/github-release.md`，范围与未完成门见 `docs/open-source-readiness.md`。
安全报告见 `SECURITY.md`。

## 许可证

MIT。见仓库 `LICENSE`。
