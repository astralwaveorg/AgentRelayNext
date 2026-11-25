# 0.1.25 复核与再审计（WP1）

**基线**：长期分支 `goal/0.1.25-release-closure`，起点 `6729c88`（`v0.1.25` 已发布）。
**方法**：对原审计报告、详细执行计划、goal 提示词三份文档的每条结论，回到当前代码取证；
判定分为 `已验证` / `部分成立` / `已过期` / `错误` / `无法静态验证` / `设计建议` / `已修复`。
**本工作包未修改任何源码、测试或构建产物**，只新增本报告。

## 一、结论台账

| ID | 原结论 | 判定 | 证据（符号/文件） | 建议工作包 |
|---|---|---|---|---|
| P0-1 | 公开 Quick Start 无法建立默认服务 | **部分成立** | `README.md` 要求拖到 `Applications`，下一步用 `$HOME/Applications/...`；DMG 不执行初始化；`local-preview` 仅实现 `prepare`（`bin/local_preview/mod.rs`），`submit` 依赖已有 `baseline`；UI 只连既有 socket | A1、A8、A9 |
| P0-2 | 发布门禁允许不可验收资产上线 | **部分成立（部分已修复）** | `release.yml` 仍设 `RELAY_NO_EXEC=1`；`build-dmg.sh` 在该分支只读 `version.json`；`prerelease:false`、稳定命名未做。已修复：`workflow_dispatch` 已移除、`overwrite_files:false`、`fail_on_unmatched_files:true`、manifest 上传 glob（`6729c88`） | A2 |
| P1-1 | 组长退出后只阻断、不回收进程组 | **成立** | `service.rs::settle_after_leader_exit`（约 1817 行）仅在 `confirm_group_exit` 成功时收口；`reclaim_group(pgid, term_budget, kill_budget)` 已存在（约 3561 行）但未被该路径调用 | A3 |
| P1-2 | diff 对特殊文件/巨型文件无界读取 | **成立** | `relay-git/src/diff.rs` 约 421/427 行先 `std::fs::read`；447 行新增文件 mode 固定 `100644` | A4 |
| P1-3 | Git 全局/系统配置仍可引入程序执行 | **成立** | `relay-git/src/git_layer.rs` 使用 `Command::new("git")`，未设 `GIT_CONFIG_NOSYSTEM`/`GIT_CONFIG_GLOBAL`/`GIT_ATTR_NOSYSTEM`，未清理注入变量；`baseline.rs` 已改为只读 `--local`（此前修复保留） | A5 |
| P1-4 | 独立回退脚本与现行升级备份不兼容 | **成立** | `packaging/rollback.sh` 查 `.backup/<stamp>`；`packaging/upgrade-local-preview.sh` 用 `.relay-backup.XXXXXX` | A6 |
| P1-5 | 目录与真实路线契约冲突 | **成立** | `model_catalog.rs` 条目 `production:true`，`recommend()` 返回 `max_rework:1`；`service.rs` 真实路由设 `max_rework=0` | A7、B5 |
| P1-6 | UI 状态与可访问性未达生产规格 | **成立** | 菜单文字取自 `model.overview`（`RelayTaskWindow.swift` 约 1486 行），连接状态另存；`accessibilityElement(children: .combine)` 出现在 1148/1183/1220/1386 行 | A8 |
| P2-1 | 新增文件 patch mode 固定 `100644` | **成立** | `diff.rs:447` | A4 |
| P2-2 | 验证读取线程超时后不取消管道 | **部分成立** | `verify.rs::spawn_pipe_reader` 已改为持续读到 EOF、缓冲有界，主流程 EOF 等待有上限；但孙进程永远持有管道时，reader 线程仍会阻塞不退出 | A3/A4 附项 |
| P2-3 | Git 基线只接受 40 位 OID | **成立** | `relay-store/src/store.rs` 约 2064 行校验 `value.len() == 40`，未支持 64 位 SHA-256 OID | A4/A5 附项 |
| P2-4 | UI demo 使用无界 `readDataToEndOfFile()` | **成立** | `RelayTaskWindow.swift` 457/503/983 行 | A8 |
| P2-5 | 开源清单状态过期 | **成立** | `docs/open-source-readiness.md` 仍写“公开范围 未授权”，T13/T21 勾选项未更新 | A9 |

## 二、计划可执行性核查

| 工作包 | 引用的文件/命令 | 核查结论 |
|---|---|---|
| WP0 | `git rev-parse/status/log`、worktree | 可执行；基线已从 `ca9525b` 漂移到 `6729c88`，按计划“基线漂移正常”处理 |
| A1 | `bin/local_preview/mod.rs`、`bin/relayctl.rs` | 文件存在；`prepare` 已实现，`enable/status/disable` 未实现 |
| A2 | `release.yml`、`package.sh`、`build-dmg.sh`、`packaging/release/` | 文件存在；`verify-assets.py`、`smoke-native.sh` 尚不存在（待新建）；发布属性部分已在本会话提前修复 |
| A3 | `service.rs`、`tests/m1_recovery.rs` | 文件存在；`reclaim_group` 实际签名与计划注一致（`pgid, term_budget, kill_budget`） |
| A4 | `diff.rs`、`tests/worktree_diff.rs` | 文件存在 |
| A5 | `git_layer.rs`、`worktree.rs`、`baseline.rs` | 文件存在；基线已只读本地配置，计划中的 global/system 禁用仍待做 |
| A6 | `install.sh`、`upgrade-local-preview.sh`、`rollback.sh` | 三个文件均存在（`upgrade-local-preview.sh` 在 `packaging/`） |
| A7 | `model_catalog.rs`、`service.rs`、`docs/model-catalog.md` | 文件存在；`CATALOG_VERSION=1`，计划建议升 2，需评估客户端兼容后再定 |
| A8 | `ui/RelayTaskWindow/RelayTaskWindow.swift` | 文件存在 |
| A9 | README/CHANGELOG/docs、`scripts/check`、`relayctl mcp-verify` | 均存在 |
| B3 | 计划写 `OpenCode Go + glm-5.3-flash` | **已被取代**：用户决定使用 `deepseek-v4.1-flash` + `RELAY_GO_MODEL`（0.1.25 已实现） |
| C1–C6 | 签名/公证/干净用户/断电等 | 依赖人类资源，按外部门禁处理 |

## 三、新增发现

1. **`v0.1.25` Release 资产缺 ZIP manifest**：`SHA256SUMS` 列出 `RelayTaskWindow-0.1.25-<build>.manifest.json`，但 Release 未上传该文件，逐项校验会在该行失败。`release.yml` 已在 `6729c88` 修复上传 glob，但已发布版本无法回补（除非手动上传）。
2. **`v0.1.24` Release 保留两套 ZIP**（`20261124130844` 与 `20261124131525`），与“同版本一套资产”目标不符；`v0.1.25` 已收敛为一套。
3. **B3 路线已被取代**：`glm-5.3-flash` 在工作区被禁用属用户个人配置；评审默认改为 `deepseek-v4.1-flash`，并支持 `RELAY_GO_MODEL` 覆盖。计划 A7/B3 文档需同步。
4. **版本号冲突**：计划目标 `0.1.25` 已发布且 tag 不可变 → 本轮收口发布目标应为 **`0.1.26`**，并按计划以 `prerelease=true` 发布。
5. **真实双账户证据已具备一次**：2026-11-24 合成样本完整通过（实现 MiniMax CN + 评审 OpenCode Go，Go 余额扣费 0，MiniMax 归属 `unverified`）。B2/B3 可标记“已完成（证据在案）”，B4/B5/B6 仍开放。
6. **审计源文件含个人路径**：三份原文档来自本机会话记录（未入库）。本报告只引用结论，未复制原始路径与内容。

## 四、远端发布现状（只读核对）

| 版本 | draft | prerelease | 资产情况 | 校验问题 |
|---|---|---|---|---|
| `v0.1.24` | false | false | 两套 ZIP、一套 DMG、SBOM、notes、`SHA256SUMS` | 缺 ZIP manifest；两套 ZIP |
| `v0.1.25` | false | false | 一套 ZIP、一套 DMG（含 sidecar 与 manifest）、SBOM、notes、`SHA256SUMS` | 缺 ZIP manifest；其余逐项可校验 |

当前 `release.yml`：仅 `v*` tag 触发；`overwrite_files:false`；`fail_on_unmatched_files:true`；包含 `RelayTaskWindow-*.manifest.json`。仍保留 `RELAY_NO_EXEC=1`，`prerelease` 仍为 `false`，资产名仍带 build 时间戳。

## 五、执行调整（相对计划默认值）

- 发布目标：`v0.1.26`（计划写 0.1.25 已不适用），`prerelease=true`。
- B3 固定组合改为 `OpenCode Go + deepseek-v4.1-flash`（`RELAY_GO_MODEL` 可覆盖）。
- B2/B3 以既有真实证据登记完成；B4 故障矩阵与候选复验按用户授权使用既有专用 key。
- P2-3（40 位 OID）并入 A4/A5 的小项，不单开工作包。
