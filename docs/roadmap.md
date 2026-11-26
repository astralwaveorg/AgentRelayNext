# 后续开发方案（v0.1.28 到 v1.0）

这份方案基于当前 `AgentRelayNext` 代码库（约 4.4 万行 Rust 和 SwiftUI）和 2026 年的开源工具格局，先拆解现状与瓶颈，再列出分阶段改造计划。

## 一、市场定位与差异化

### 1. 同类工具现状

| 品类 | 代表项目 | 强项 | 缺口 |
| --- | --- | --- | --- |
| 并行 Worktree 编排壳 | Conductor、Agentastic、Claude Squad、Vibe Kanban | 同时运行多个 Agent 会话，看板或 TUI 切换顺手 | 缺少跨模型独立评审和不可篡改证据，往往要求用户换掉原来的主入口 |
| 独立代码评审工具 | CodeRabbit、Greptile、PR-Agent、CodeTurtle、Plannotator | 专职审计划或审 Diff | 只管评审，不管隔离执行、测试收据绑定和自动返工 |
| 主流客户端内置能力 | Claude Code、Codex、Cursor 原生 Worktree 与 Subagents | 零额外安装，交互连贯 | 同模型自审容易放过同类盲区，也绑定各自模型和计费 |

### 2. Agent Relay 的切入点

Agent Relay 定位为**退居后台的 MCP 本地质量门禁与降本中继器**。用户继续在习惯的 Codex、Claude Desktop 或终端里发起需求。后台由高性价比或本地模型在隔离 worktree 中改代码、跑固定本地测试，再交由不同工具和模型做只读评审和自动返工，最后把 SHA-256 绑定的证据包、一键落分支操作和省钱账单交回给用户。

这个切入点对应两类直接需求：

1. 质量与可控性。代码修改不碰主工作区，测试真实跑过并留收据，评审由独立模型把关，解决“AI 改完到底敢不敢合”的问题。
2. 混合模型降本。旗舰模型只做规划和终验，代码实现与自测交给 DeepSeek、MiniMax、GLM、Kimi 或本地 Ollama，把单任务成本压下来，并用实测账单量化节省比例。

## 二、当前代码诊断

### 1. 值得保留的底座资产

现有 6 个 Rust crate 和 SwiftUI 外壳构成一套很稳的基础设施，不需要重写：

- `relay-git` 和 `relay-sys` 提供了 `/usr/bin/git` 执行面加固、有界 diff、写入白名单、detached worktree、进程组 `TERM → KILL` 回收和同 UID Unix socket。
- `relay-core` 和 `relay-store` 提供了 SQLite schema v9（WAL）、CAS 状态机、幂等去重、启动栅栏、实例级派发门、`stage_runs` 阶段表和原子制品发布。
- `ui/RelayTaskWindow` 和 `packaging/` 提供了原生菜单栏与任务窗口、MCP stdio 桥、SBOM、资产校验、原生 smoke 和升级回滚脚本。

### 2. 限制真实使用的 5 个短板

| # | 短板 | 代码位置 | 实际影响 |
| --- | --- | --- | --- |
| 1 | 供应商与计费逻辑硬编码 | `crates/relay-daemon/src/live_routes.rs`、`route_pilot.rs` | 绑定 MiniMax 和 OpenCode Go 特定端点与 `GO_USAGE` 探测，外部开发者没有同一套账号就无法开箱使用 |
| 2 | 实现阶段能力过窄 | `live_routes.rs:21-23`、`route_pilot.rs:821-842` | 禁用 CLI 工具（`--tools ""`），限制 64 KiB 输入，只接受单轮整文件 JSON 替换且 `max_rework=0`，扛不住真实跨文件需求 |
| 3 | 缺少验收后落地动作 | `ui/RelayTaskWindow/`、`service.rs` | 验收通过后只更新数据库状态，没有一键创建分支应用补丁或导出 PR 证据卡片 |
| 4 | 单实例单项目且串行 | `service.rs` | 每个项目都要单独启停服务，多仓库切换和并行子任务受限 |
| 5 | `0.1.28` 遗留缺陷 | `live_routes.rs:197,295`、`relay-daemon-wrapper.sh:27-35`、`service.rs` | 实现模型硬编码 `MODEL` 漏接、后台 `mv` 日志轮转 FD 失效、A10 gate 终态收敛和 A11 `killpg` 归属复核尚未收口 |

## 三、四项核心架构转型

### 1. 开放 Provider 与统一 `AgentAdapter` 协议层

把 `live_routes.rs` 和 `route_pilot.rs` 中的供应商特化逻辑拆成独立 crate `crates/relay-adapters/`，引入声明式的执行配置档（`~/.agent-relay-next/profiles.toml`）：

```toml
[profiles.writer-deepseek]
kind = "cli"                        # cli | openai_api | anthropic_api
tool = "opencode"                   # claude-code | codex | opencode | kimi-code | codebuddy
model = "deepseek/deepseek-v4.1-flash"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"    # 或 keychain:<entry>，不存明文

[profiles.reviewer-claude]
kind = "cli"
tool = "claude-code"
model = "claude-sonnet-4-6"
api_key_env = "ANTHROPIC_API_KEY"
read_only = true

[profiles.local-ollama]
kind = "openai_api"
model = "qwen3-coder:32b"
base_url = "http://127.0.0.1:11434/v1"
```

子进程启动时按 Profile 注入临时环境变量（如 `ANTHROPIC_BASE_URL`、`OPENAI_BASE_URL`），不修改用户全局登录态。独立性检查继续复用 `normalize_model()`，只要双方模型不同即允许配对。原有 MiniMax + OpenCode Go 路线保留为内置 Preset，兼容现有配置。

### 2. 双模式实现引擎与自动返工

拆分两种显式执行模式，满足不同安全等级与任务复杂度：

| 模式 | 适用场景 | 执行方式 | 外层不变的安全底线 |
| --- | --- | --- | --- |
| `structured`（现有） | 极小改动，要求最小数据出境 | 禁用工具，白名单文件进单轮 JSON 替换 | 写入白名单、本地验证、只读评审、SHA-256 绑定 |
| `agentic_worktree`（新增默认） | 真实跨文件开发、修 Bug、补测试 | 允许实现 Agent 在受管 worktree 内读仓、局部编辑并自跑测试 | 结束后由 `relay-git` 审计 `write_scope` 越界、重跑固定本地验证生成收据，再送独立只读评审 |

同时开放真实路线自动返工（`max_rework` 默认 `1`，范围 `0` 到 `3`）。第 1 轮评审打回且给出可执行 findings 时，把精简后的 `review.json` 传回实现 Agent 在同一 worktree 内返工，复用 `workflow.rs` 已有的 `StageRun` 多轮状态机。

### 3. 验收后一键落地与成本账本

验收通过后仍需人工显式触发落地，但把重复操作缩成一步：

- `relayctl apply <task-id> --branch relay/<slug>`（以及窗口内一键按钮）：校验 `diff_sha256` 和 `base_oid` 后基于基线创建新本地分支并 `git apply`，不碰当前脏工作区。
- 一键复制 Markdown PR 证据卡片：包含任务目标、改动文件、本地测试收据摘要、评审模型、返工轮次和结论，可直接贴进 Pull Request 描述。
- 成本与省钱账本（Cost Ledger）：记录各阶段 token、缓存命中和参考费率，对比混合路由实付成本与全程旗舰模型估算成本，在菜单栏和详情页展示节省比例。

### 4. 极简上手与多项目管理

- 新增 `relayctl init` 向导：扫描本机已安装的 CLI（`claude`、`codex`、`opencode`、`kimi`、`codebuddy`）和环境变量，生成 `profiles.toml` 与项目 `.relay.toml`，并输出可直接粘贴到 Codex 或 Claude Desktop 的 MCP 配置。
- 单 daemon 管理多项目：常驻服务按 `project_id` 路由到各自独立的 `repo_root` 和 `managed_root`，任务窗口直接切换项目。
- 提供 Homebrew Tap（`brew install --cask astralwaveorg/tap/agent-relay`），降低 macOS 用户安装和升级门槛。

## 四、分阶段开发路线图

### Phase 0：`v0.1.28` 底座封版（目标：1 周）

目标是在修完已知确定性缺陷后封存 `0.1.x` 底座线，停止继续堆砌纯防御性细节。

1. 修复 `live_routes.rs`（L197 和 L295）实现模型硬编码 `MODEL` 漏接，以及 `live-project-acceptance.py` 环境变量透传。
2. 修复 `packaging/relay-daemon-wrapper.sh` 运行期日志轮转 FD 失效（改用 copytruncate）。
3. 补齐 UI 状态投影自动化测试（抽离 `MenuStatus.swift`，同步更新 `build-app.sh` 并接入 `scripts/check`）。
4. 完成 A11（`killpg` 前进程组归属复核，兼容组长退出后代残留并测试伪造/复用 pgid 零发信号）。
5. 完成 A10（`run_attempt` gate 终态收敛，统一 `review_active` 与 `implementation_only` 出口）。
6. 执行 B4 四项真实负例与 `0.1.28` 正例脱敏取证，发布 `v0.1.28` prerelease。

验收标准：`./scripts/check full` 与 CI 全绿，`smoke-native.sh` 与 `rehearse-upgrade.sh` 通过，`v0.1.28` Release 资产 `SHA256SUMS` 全量通过。

### Phase 1：`v0.2.0` 开放 BYOK 与多 CLI 适配（目标：2 到 3 周）

目标：任意开发者用自己的两家模型或 CLI，在 5 分钟内跑通真实仓库任务。

1. 新建 `crates/relay-adapters/` 与 `profiles.toml` 加载器，实现 `OpenAiCompatibleAdapter`、`AnthropicCompatibleAdapter`、`ClaudeCodeCliAdapter`、`OpenCodeCliAdapter` 和 `CodexCliAdapter`。
2. 支持项目级 `.relay.toml` 与 `relayctl init` 初始化向导，自动生成 MCP 配置片段。
3. 在 SwiftUI 窗口加入 Profile 列表和连通性自检（支持环境变量和可选 Keychain 引用，不存明文密钥）。

验收标准：至少 3 组不同真实组合跑通端到端验证（如 `Codex + Claude Code`、`Claude Code + DeepSeek API`、`OpenCode + 本地 Ollama`），主流程无供应商硬编码。暂不做 Windows/Linux GUI 和跨工具配置同步。

### Phase 2：`v0.3.0` 多轮 Worktree 编码与自动返工（目标：3 到 4 周）

目标：能独立处理真实仓库的中等复杂度功能与缺陷修复。

1. 实现 `agentic_worktree` 执行模式，允许实现 CLI 在受管 worktree 内多轮读写与自测，结束后由 `relay-git` 统一拦截越界路径并重跑固定验证。
2. 开放真实路线 `max_rework` 自动返工闭环，将结构化 findings 喂回实现阶段。
3. 新增受控实时进度流，让 SwiftUI 窗口和 `relayctl watch <task-id>` 实时显示阶段耗时与一键取消。
4. 支持单常驻 daemon 管理多个已登记项目。

验收标准：用 Agent Relay 自身开发并合入至少 5 个真实仓库 PR，且包含至少 2 个评审发现真实缺陷后自动返工通过的案例。

### Phase 3：`v0.4.0` 交付闭环、成本账本与分发（目标：2 到 3 周）

目标：打通验收到 PR 的最后一公里，具备完整开源传播条件。

1. 实现 `relayctl apply --branch` 与窗口内一键创建本地分支、一键复制 PR 证据卡片。
2. 上线成本与省钱 ROI 账本，展示每次任务的实际花费与相对全程旗舰模型的节省比例。
3. 建立 Homebrew Tap，并补充 30 秒真实工作流录屏和 Dogfooding 案例文档。

验收标准：新用户从安装到在自己的 Git 仓库跑通首个双模型任务不超过 5 分钟。

### Phase 4：`v1.0.0` 并发调度与生态扩展（按社区反馈推进）

- 多 Worktree 并发调度，结合互不重叠的 `write_scope` 锁并行跑子任务。
- 接入标准 ACP 原生适配器（如 `kimi --acp`、`codebuddy --acp`、`codex-acp`）。
- 提供 Linux 和 SSH 环境可用的无头 `relayctl` 与 `relaytop`（Ratatui TUI）。
- 视社区需求评估统一配置同步（`AGENTS.md`、`CLAUDE.md`、MCP 同步）以及 Apple Developer ID 签名公证。

## 五、执行纪律与开源推广节奏

1. 从 `v0.2.0` 起坚持 Dogfooding 驱动，新需求优先来自日常真实仓库使用中暴露的卡点。
2. 守住 4 条不可妥协的安全红线：主工作区零触碰、实现与评审模型真独立、制品与收据 SHA-256 绑定、零凭据落盘与零网络监听。其余非核心边缘项按实际需要迭代。
3. 文档保持单一事实源，版本流水账统一只写进 `CHANGELOG.md`。
4. 大规模对外推广安排在 `v0.2.0` 或 `v0.3.0` 完成后进行。首发配合一条 30 到 45 秒真实录屏（MCP 下单、独立评审挑错、自动返工通过、一键生成带证据分支与省钱账单），同步发布到 GitHub、Hacker News、Reddit（`r/ClaudeCode`、`r/LocalLLaMA`、`r/rust`）、V2EX 和掘金。
