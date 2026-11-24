# Agent Relay 0.1.25 — 本机开发预览

**One coding agent writes. Another reviews. You get the evidence.**

这是 Agent Relay 的本机开发预览，不是签名/公证的正式 macOS 发行版。DMG 未签名，
首次启动需要手动放行；不要关闭 Gatekeeper。

## 下载

- `RelayTaskWindow-0.1.25.dmg`
- 校验：下载同目录的 `RelayTaskWindow-0.1.25.dmg.sha256`，或使用 Release 附件中的
  `SHA256SUMS`；构建号见 `.dmg.manifest.json` 和 `RelayTaskWindow-*.zip` 文件名。
- 构建：product `0.1.25`，业务 `relay.v1`，wire `1`，schema `9`

## 本版内容

- **MiniMax 中国大陆站**：`RELAY_MINIMAX_ENDPOINT` 在预览和运行前选择固定端点
  （国际站 `api.minimax.io` 为默认，中国大陆站 `api.minimax.cn`）；其他值直接失败，
  approval 绑定实际端点。
- **可配置评审模型**：`RELAY_GO_MODEL` 选择 OpenCode Go 评审模型（默认
  `deepseek-v4.1-flash`）；approval 绑定实际模型，且不得与实现模型相同。
- **修复**：Claude Code 2.1.326 在模型 thinking 时发出的 `system/thinking_tokens`
  进度帧不再被误判为未知帧。
- **文档**：真实路线示例改用 `MINIMAX_API_KEY` / `OC_API_KEY` 环境变量提供两把专用
  key，不再依赖钥匙串查询。
- **真实双账户合成样本**已在本候选上通过一次：MiniMax `MiniMax-M3.1-Flash` 实现
  （中国大陆站端点，扣费归属 `unverified`）→ 本机验证 → OpenCode Go
  `deepseek-v4.1-flash` 只读评审（`balance_charge_usd=0`，供应商来源 `go`）。

## 安装

1. 核对下载摘要：

   ```sh
   shasum -a 256 RelayTaskWindow-0.1.25.dmg
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
   所有者确认套餐、专用凭据与工作区已启用的模型。

## 验证摘要

- `./scripts/check full`：236 passed / 0 failed / 0 ignored。
- 真实双账户合成样本一次通过：单 task、单 attempt，implement/verify/review 三阶段
  均为 succeeded，补丁可在独立副本应用且原合成仓库未变。
- doctor、MCP 8 项工具与离线演示由本机与 CI 检查覆盖。
- DMG 挂载、App/relayctl 存在性和版本校验通过；没有 Developer ID/公证/Gatekeeper
  正例。

## 已知限制与未完成门

- **未签名、未公证**：没有 Apple 开发者账号，不提供 Developer ID、notarization、
  stapled ticket 或 Gatekeeper 正例；只提供未签名 DMG 开发预览。
- 真实双账户证据只有一次合成样本；长期额度、并发、限流、取消与故障矩阵未验证。
  MiniMax 扣费归属没有可靠回传，仍标记 `unverified`；OpenCode Go 结果只代表本次
  工作区配置。
- 干净 macOS 用户、第二 UID、受管断电、菜单栏/可访问性运行期验收未完成。
- 性能 P95 ≤100 ms 目标未在空闲参考机证明。
- 模型目录只含已验证的本机路线；目录升级、别名重定向、型号退役和预授权备用路线
  仍未实现。
- 工作副本不是操作系统沙箱；只服务可信本机仓库和可信本机用户。

构建与安装见 `docs/github-release.md`，范围与未完成门见 `docs/open-source-readiness.md`。
安全报告见 `SECURITY.md`。

## 许可证

MIT。见仓库 `LICENSE`。
