# Agent Relay 0.1.26 — 本机开发预览

**One coding agent writes. Another reviews. You get the evidence.**

这是 Agent Relay 的本机开发预览，不是签名/公证的正式 macOS 发行版。DMG 未签名，
首次启动需要手动放行；不要关闭 Gatekeeper。

## 下载

- `RelayTaskWindow-0.1.26-macos-arm64.zip`（含 manifest 与 SHA-256）
- `RelayTaskWindow-0.1.26.dmg`
- 校验：下载同目录的 `.sha256`，或使用 Release 附件中的 `SHA256SUMS`；构建号见
  `.dmg.manifest.json` 与 ZIP manifest。
- 构建：product `0.1.26`，业务 `relay.v1`，wire `1`，schema `9`

## 本版内容

- **首次运行更简单**：安装后可在任务窗口点击“启用本地预览”，或运行
  `relayctl local-preview enable`；它创建 `~/.agent-relay-next`、安装稳定包装脚本、
  注册当前用户 LaunchAgent，并跑 doctor 与 MCP 握手。`status`/`disable` 可用于检查与
  安全停用；停用默认保留数据。
- **发布管线加固**：一个版本一套稳定命名资产；`verify-assets.py` 生成并逐项校验
  `SHA256SUMS`；`smoke-native.sh` 在临时 HOME 跑 prepare → daemon → doctor → MCP →
  离线 demo → 摘要核对 → DMG 挂载；发布仅由 tag 触发，不覆盖同名资产。
- **进程与资源边界**：组长退出后按 TERM → SIGKILL 有界回收整组；diff 增加单文件/
  总量/补丁/Git 输出/总时限上限，FIFO 等特殊文件不读取，新增文件保留执行位。
- **Git 执行面**：固定 `/usr/bin/git`，禁用 system/global 配置与系统级 attributes，
  强制关闭 fsmonitor/hooks/attributes，清理 `GIT_CONFIG_*` 注入；基线拒绝本地可执行面
  与启用的 `filter=` 属性。
- **统一备份与回退**：install/upgrade/rollback 共用 `.relay-backup.*` 与
  `metadata.json`；`rollback.sh` 校验应用与快照摘要、先备份当前状态、同 schema 直接
  还原，v9→v8 走既有 `rollback-v8`，其他降级拒绝。
- **目录与 UI 契约**：推荐结果明确 `production_ready=false`、`preview_only=true`、
  `max_rework=0`；菜单文字与图标同源并提示过期状态；演示子进程有超时与输出上限；
  验收卡片向 VoiceOver 暴露实现者、评审者与 findings。

## 安装

1. 核对下载摘要：

   ```sh
   shasum -a 256 RelayTaskWindow-0.1.26-macos-arm64.zip RelayTaskWindow-0.1.26.dmg
   ```

2. 打开 DMG，把 `RelayTaskWindow.app` 拖到 `Applications`。
3. 首次启动若被 macOS 阻止：Control-click App → 打开，或到“系统设置 → 隐私与安全性”
   选择“仍要打开”。这是未签名/未公证包的预期行为。
4. 首次运行点击“启用本地预览”，随后用包内 CLI 检查：

   ```sh
   relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
   "$relayctl" local-preview status
   "$relayctl" doctor --instance-dir "$HOME/.agent-relay-next/instance"
   ```

5. 离线演示不调用真实模型；真实路线需要安装 Git、Claude Code/OpenCode，并由账户
   所有者确认套餐、专用凭据与工作区已启用的模型。

## 验证摘要

- `./scripts/check full`：本机全量通过；CI 记录见本次 tag 的 Release 运行。
- 原生 arm64 smoke：prepare → daemon → doctor → MCP → 离线 demo → 摘要核对 → DMG
  挂载，本机与 CI 均通过。
- **真实双账户合成样本在本候选上通过**：MiniMax `MiniMax-M3.1-Flash`（中国大陆站，
  input 254 / output 99，扣费归属 `unverified`）→ 本机验证 → OpenCode Go
  `deepseek-v4.1-flash`（input 306 / output 267，`balance_charge_usd=0`，来源 `go`），
  单 task / 单 attempt 到达 completed。首次尝试遇到一次瞬时 `UPSTREAM_TRANSPORT`，
  按一次性规则改用新样本复验通过。
- 资产：`SHA256SUMS` 对全部附件逐项可校验；同版本只有一套 ZIP 与一套 DMG。
- DMG 挂载、App/relayctl 存在性与版本校验通过；没有 Developer ID/公证/Gatekeeper
  正例。

## 已知限制与未完成门

- **未签名、未公证**：没有 Apple 开发者账号，不提供 Developer ID、notarization、
  stapled ticket 或 Gatekeeper 正例；只提供未签名 DMG 开发预览。
- 真实双账户证据来自受控合成样本；长期额度、并发、限流、取消与故障矩阵未全部覆盖。
  MiniMax 扣费归属仍标记 `unverified`；OpenCode Go 结果只代表本次工作区配置。
- 干净 macOS 用户、第二 UID、受管断电、菜单栏/VoiceOver 运行期验收仍需人工完成。
- 性能 P95 ≤100 ms 目标未在空闲参考机证明。
- 模型目录仍只含已验证的本机路线；目录升级、别名重定向、型号退役与预授权备用路线
  未实现。
- 工作副本不是操作系统沙箱；只服务可信本机仓库和可信本机用户。

构建与安装见 `docs/github-release.md`，范围与未完成门见 `docs/open-source-readiness.md`。
安全报告见 `SECURITY.md`。

## 许可证

MIT。见仓库 `LICENSE`。
