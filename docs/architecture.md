# 架构与运行边界

本文描述当前 `0.1.x` 代码实际实现的架构，不把设计目标写成已完成能力。验收矩阵和
未完成门见[发布说明](release-notes-0.1.23.md)和[开源准备清单](open-source-readiness.md)。

## 进程与数据流

```text
Host / Codex ── MCP stdio ──> relayctl mcp ── local Unix RPC ──┐
Shell ─────────────────────── relayctl ───────────────────────┤
SwiftUI 任务窗口 ─────────── local Unix RPC ──────────────────┤
                                                             ▼
                                                        relay daemon
                                                             │
                     ┌───────────────────────────────────────┼──────────────────────┐
                     ▼                                       ▼                      ▼
                  SQLite                                 managed worktree       verification
              plans/tasks/events                     Git detached checkout    fixed argv, no shell
              stage_runs/artifacts                   diff + receipt           process group
```

- **daemon** 是唯一业务写入者。MCP 桥、CLI 和窗口都只通过本机 socket 调用它。
- **SQLite** 保存计划、请求去重、任务/attempt、事件、阶段、派发门、受管进程组和
  已登记制品。制品文件本身在受管目录，数据库只保存引用和摘要。
- **worktree** 从固定 commit OID 创建；实现者不直接获得主 checkout 的写权限。
- **验证**只执行可信配置中的固定可执行文件和 argv，工作目录是受管 worktree，
  环境清空，不经 shell。
- **发布**先写同卷暂存目录、逐文件同步和回读，再以不覆盖目标的原子改名发布；
  数据库登记与状态推进在同一事务。

## 模块

| crate / 文件 | 责任 |
| --- | --- |
| `relay-core` | 纯状态机：合法转换、恢复分类、重试资格 |
| `relay-protocol` | 256 KiB 限长帧、UTF-8、重复键扫描、hello v1、DTO、稳定错误 |
| `relay-store` | schema v9、WAL、同步写、CAS、幂等、启动栅栏、阶段、制品登记 |
| `relay-git` | 路径授权、只读基线、受管 worktree、diff、收据、原子制品 |
| `relay-daemon` | 业务编排、进程组监管、IPC、MCP 桥、CLI、项目登记和维护入口 |
| `relay-sys` | `renamex_np`、`getpeereid`、进程组、磁盘空间等 macOS 原语 |
| `ui/RelayTaskWindow` | SwiftUI 窗口、菜单栏、只读总览、差异/收据/验收、项目登记 |
| `packaging/` | 本机预览打包、安装、升级、回滚、预演和发行验证脚本 |

## 任务状态

`queued → preparing → launching → running → verifying → awaiting_review → completed`
（或 `rejected`）。失败、取消、阻断和中断分别有独立状态；`cancelling` 只有在确认
受管进程组退出后才能进入 `cancelled`。启动许可在 spawn 前持久化，同一 attempt 不会
获得第二次启动许可。

恢复时 `preparing/launching/running/verifying/cancelling` 一律归入 `interrupted`，
不自动重放；`awaiting_review` 和终态保持。无法确认进程退出时派发门保持阻断，必须
有可信证据才解除。

## 自动评审链

每个 attempt 内按轮次写入持久 `stage_runs`：

```text
round 1: implement → verify → review
             ↑                    │
             └── needs_changes ───┘  (最多配置的返工轮次，首版默认 1)
```

- 实现者输出结构化替换；每个替换必须匹配原文件 SHA-256，全部验证通过后才应用。
- 验证失败不发送评审请求。
- 评审只收到目标、diff、验证状态/规则 ID 和两个摘要；评审报告必须绑定当前 diff
  与收据摘要。
- 评审后再次采集 diff；候选变化（评审写入或并发修改）不得放行。
- 评审通过后仍需用户显式验收；验收不会应用补丁到主分支。

## 本机安全边界

- 实例目录必须由当前 UID 拥有且权限严格 `0700`。
- Unix socket 只接受同 UID 对端；已存在但归属不明的 socket 不抢占、不删除。
- 数据库、锁、socket 和制品路径拒绝符号链接；制品读取核对登记摘要和绑定字段。
- 子进程自成进程组；取消/超时/停机先确认整组退出，未确认则保持阻断。
- 真实路线凭据只经私有 stdin 进入一次性 worker，不进入 argv、环境、日志或持久配置。
- 工作副本不是操作系统沙箱；首版只服务可信本机仓库和可信本机用户。

## 明确未完成

- 真实双账户端到端链路尚未取得本轮运行证据。
- 通用模型目录/推荐、预授权备用路线、目录升级尚未实现。
- 2 GiB 制品/工作树配额、保留清理和日志轮转压力测试尚未完整实现/验证。
- 正式签名、公证、Gatekeeper、干净用户、第二 UID 和真实断电验收未完成。
- 菜单栏完整浮层、通知、VoiceOver 全量场景未做运行期验收。
