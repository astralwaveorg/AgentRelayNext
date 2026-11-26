# 架构与安全边界

本文描述 Agent Relay 当前 `0.1.x` 代码实际实现的架构与运行不变量。后续 `0.2+` 的架构调整见 [后续开发方案](roadmap.md)。

## 1. 进程拓扑与数据流

```text
Host / Codex ── MCP stdio ──> relayctl mcp ── local Unix RPC ──┐
Shell ─────────────────────── relayctl ───────────────────────┤
SwiftUI 任务窗口 ─────────── local Unix RPC ──────────────────┤
                                                             ▼
                                                        relay daemon
                                                             │
                    ┌────────────────────────────────────────┼──────────────────────┐
                    ▼                                        ▼                      ▼
                 SQLite                               受管 worktree              本地验证
           plans / tasks / events                 detached Git checkout     固定 argv，无 shell
           stage_runs / artifacts                 有界 diff + 测试收据       进程组监管与回收
```

- `relay-daemon` 是唯一的业务写入者。CLI、MCP 桥和 SwiftUI 窗口都只通过本机 Unix socket 发送有界 JSON-RPC 2.0 请求，不直接写数据库或拉起任务进程。
- `relay-store` 维护 SQLite schema v9（WAL 模式），存储计划快照、请求去重表、任务和 attempt 状态、事件流、`stage_runs`、启动栅栏（`launch_fences`）、派发门（`dispatch_gate`）、受管进程组（`managed_groups`）与制品索引。大文件保存在受管目录，数据库只保存路径引用和 SHA-256 摘要。
- `relay-git` 从固定基线 commit OID 创建 detached Git worktree，实现过程不会改动主 checkout 的 `HEAD`、暂存区或工作区文件。
- 本地验证按登记的固定可执行文件和 `argv` 在受管 worktree 内运行，清空环境变量且不经 shell，并对输出做脱敏与限时控制，产出结构化 `test_receipt.json`。
- 制品发布先写同文件系统暂存目录，落盘回读计算摘要，再用 `renamex_np(RENAME_EXCL)` 执行不替换原子改名。随后在单个 SQLite IMMEDIATE 事务内完成制品登记和状态推进。

## 2. 模块职责边界

除 `relay-sys` 集中封装少量 POSIX 和 macOS 平台调用外，其余 Rust crate 均启用 `#![forbid(unsafe_code)]`。

| 模块 | 职责 |
| --- | --- |
| `crates/relay-core` | 纯状态机：合法状态转换表、重启恢复分类、重试资格判定 |
| `crates/relay-protocol` | 256 KiB 限长帧解码、UTF-8 校验、重复 JSON 键预扫描、`hello v1` 握手、DTO 与稳定错误码 |
| `crates/relay-store` | SQLite schema v9 全表与全列核对、WAL、CAS 转换、请求去重、启动栅栏、派发门与阶段表 |
| `crates/relay-git` | 写入路径白名单、加固 `/usr/bin/git` 调用面、worktree 管理、有界 diff（8/32/16/32 MiB 和 30 s）、测试收据、原子制品 |
| `crates/relay-daemon` | 任务编排器、进程组监管回收、Unix socket IPC、MCP stdio 桥、项目登记、模型目录、维护命令 |
| `crates/relay-sys` | 最小平台原语：`rename_no_replace`、`exchange_paths`、`socket_peer_euid`、`setpgid`/`killpg`、`group_has_members`、`free_space` |
| `ui/RelayTaskWindow` | SwiftUI 任务窗口、菜单栏面板、只读总览、差异/收据/交付查看、锁定摘要的验收弹窗、首启引导 |
| `packaging/` | 本地打包、未签名 DMG 构建、SBOM、资产校验、原生 smoke、升级与回滚脚本 |

## 3. 状态机与失败恢复

```text
queued → preparing → launching → running → verifying → awaiting_review → completed | rejected
```

- 取消（`cancelling → cancelled`）、失败（`failed`）、阻断（`blocked`）和中断（`interrupted`）均为显式状态。
- 启动栅栏在 `preparing → launching` 时先于 `spawn` 持久化唯一启动许可，并打开实例级 `dispatch_gate`（`pending`）。同一个 attempt 不会获得第二次启动许可。
- daemon 重启恢复时，把所有未决活跃状态（`preparing`、`launching`、`running`、`verifying`、`cancelling`）统一归入 `interrupted`，不会自动重放可能已有外部副作用的任务。`awaiting_review` 和终态保持不变。
- 只要存在未确认退出的进程组或 `pending` gate，实例就会暂停领取新任务。直到探测确认整组为空，或运维通过 `relayctl gate confirm` 写入审计记录后，派发门才会解除。

## 4. 独立评审链

每个 attempt 在 SQLite `stage_runs` 表中按轮次记录独立阶段：

```text
round N: implement ──▶ verify (local) ──▶ review (independent, read-only)
              ▲                                     │
              └────────── changes_requested ────────┘  (受 max_rework 上限控制)
```

1. 真实路线要求实现阶段输出结构化替换，逐文件核对原文件 SHA-256，全部校验通过后才写入受管 worktree。
2. 本地验证未通过（收据 `status != "pass"`）时任务直接失败，不调用评审模型。
3. 评审报告必须符合 `ReviewReport v1`（不超过 16 KiB、最多 20 条 findings）且严格匹配当前 `candidate_diff_sha256` 和 `test_receipt_sha256`。如果报告返回 `pass` 但包含 `blocking` 级别 finding，系统强制将其视为 `changes_requested`。
4. 评审结束后系统再次采集 worktree diff。如果候选发生任何变化（例如评审越权写入或外部改动），交付会被拒绝。
5. 人工验收提交前会核对磁盘制品字节与登记摘要，全部一致才推进到 `completed` 或 `rejected`。

## 5. 本机安全不变量

| 维度 | 当前实现 |
| --- | --- |
| 文件与目录权限 | 实例目录必须由当前 UID 拥有且为 `0700`，`relay.sock` 和 `.own` 绑定后设为 `0600`，关键路径拒绝符号链接 |
| IPC 身份校验 | 仅监听本机 Unix socket（无 TCP/HTTP 端口），通过内核 `getpeereid` 核验对端 EUID 与 daemon 一致 |
| Git 执行面加固 | 固定 `/usr/bin/git`，禁用 global/system config 和 attributes，强制关闭 `fsmonitor` 和 hooks，清理 `GIT_CONFIG_*` 注入，基线拒绝本地可执行面和活跃 `filter=` |
| Diff 资源边界 | 单文件 8 MiB、总量 32 MiB、补丁 16 MiB、Git 输出 32 MiB、超时 30 s，拒绝 FIFO/socket/设备文件且不读内容，保留新增文件执行位 |
| 进程组回收 | 受控子进程自成进程组，超时、取消或组长退出后执行有界 `SIGTERM → SIGKILL` 回收并确认组为空 |
| 凭据与载荷保护 | 密钥只走私有 stdin，验证输出过滤常见令牌形态（如 `sk-`、`ghp_`、`AKIA`、`bearer`、`api-key`），`tasks.reason_code` 限 256 字节去控制字符，`ui.overview` 单评审裁剪到 64 KiB 内 |

> **边界说明。** 受管 worktree 和写入白名单用于隔离主分支并限制补丁范围，但不等同于操作系统级沙箱。当前安全模型面向可信本机用户和可信本机仓库。
