# 本机试用：Agent Relay 0.1

从 Finder 打开当前用户“应用程序”中的 **RelayTaskWindow**。点击“运行离线演示”，选择左侧任务。样本会自动实现、验证、评审并返工一次。等任务变为“待验收”，点击“加载差异”，再选择“验收交付…”。验收只登记结果，不应用补丁，也不修改主分支。可重复提交样本，每次都是新任务。

0.1.4 新增“运行双工具样本”。它用本机的 Claude Code 实现、逐字节验证样本改动，再用本机的 OpenCode 评审，窗口显示三阶段结果。需要已经安装这两个 CLI。样本在临时实例运行，结果不会出现在左侧任务列表。要查看补丁、试用人工验收，请使用原来的“运行离线演示”。

两个入口都使用固定样本，不会调用真实模型、读取账户凭据或改动你的项目。“运行双工具样本”用真实 CLI，供应商响应仍由本机夹具模拟。生产路线仍需验证真实身份、权限、套餐及部署条件。

- 应用：`~/Applications/RelayTaskWindow.app`
- 数据及日志：`~/.agent-relay-next`，独立于现有 Agent Relay 和真实项目。
- 用户服务：`net.relay.candidate.local-preview`，关闭窗口时仍运行。
- 状态与排障：`~/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl doctor --instance-dir "$HOME/.agent-relay-next/instance"`
- MCP 桥：`relayctl mcp --socket "$HOME/.agent-relay-next/instance/relay.sock"`。试用安装不修改现有客户端配置。
- 暂停后台服务：`launchctl bootout "gui/$(id -u)/net.relay.candidate.local-preview"`；重新启动时对对应的 plist 运行 `launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/net.relay.candidate.local-preview.plist"`。这两步不删除数据。

从本地构建产物首次安装：运行 `packaging/package.sh <绝对输出目录>`，再把生成的 ZIP 路径传给 `packaging/install-local-preview.sh`。安装器核对摘要，拒绝覆盖现有应用或数据。对正在运行的试用实例，先关闭任务窗口并暂停其他客户端的提交，等排队与运行任务处理完毕。使用 `packaging/upgrade-local-preview.sh <新版 ZIP>` 升级。它要求产品版本不同、业务协议与 wire 相同；数据库支持同 schema，或由候选明确支持的 v8 → v9；有排队或运行任务、退出未确认，或者制品摘要不一致时，升级会停止。0.1.0 不识别维护标记，首次升级前尤其要暂停外部提交。0.1.1 会识别维护标记，升级期间拒绝写请求和领取任务，同时保留读取。

升级保留原实例和任务历史，并把原应用、经检查的 SQLite 一致快照及已登记制品放入命令输出的私有备份目录。候选先在维护状态通过 MCP 握手、版本检查和 doctor，才恢复提交。若候选启动或检查失败，安装器尝试恢复原应用和服务，并保留失败现场。跨 schema 时先迁移独立数据库，校验历史，再用同文件系统的原子交换替换活动数据库。候选启动失败会换回原库与原应用；如果看到“自动恢复未通过”或遗留升级锁，保留锁、维护标记和备份，避免重复执行。强杀升级器和真实断电尚未验证；签名公证、公开分发和把档案重新投入生产不在本命令范围中。不要在服务运行时用 `rollback.sh` 替换应用。

在独立 HOME 和临时 launchd 标签上复验：`packaging/rehearse-upgrade.sh <0.1.0 ZIP> <0.1.1 ZIP>`。演练制造一个无法启动的候选，再安装正常候选；两次都检查同一个既有任务和补丁。从 v8 验证迁移时，用 Git 基线 `906bfa3` 构建原 `relayctl`，以 `packaging/rehearse-schema-upgrade.sh <v8 relayctl> <v9 ZIP>` 实测。旧实例用 v8 daemon 创建和读取数据，UI 和离线样本提交借用新包助手。


要查看备份中的任务和制品，运行安装包内的 `relayctl maintenance restore <备份/data> <新根目录>`，再运行 `relayctl maintenance serve-archive --instance-dir <新根目录>/instance`。备份和新根目录使用物理绝对路径；新根目录必须不存在，且路径应短于 Unix socket 的长度限制。用 `relayctl status`、`artifact`，或任务窗口的 `--socket <新根目录>/instance/relay.sock` 查看。档案不启动 worker，不允许提交、取消或验收，结束前台命令即可。恢复会把制品登记重绑到档案自身，原实例和原备份均不变。新版备份带数据库 SHA-256；从 0.1.1 的旧备份恢复时，只能核对结构、引用、计数和制品摘要，没有原始数据库摘要可核。

跨 schema 升级留下的 v8 快照是回退证据；本版本的档案服务不会把 v8 快照自动当成 v9 档案读取。

0.1.3 增加 [真实路线资格核验入口](route-pilot.md)。它可离线验证 CLI，也可由账户所有者明确授权一次非敏感样本；不会打开真实任务派发。

0.1.5 增加 OpenCode 评审路线资格核验，并修正 MCP 的业务错误标记、补齐工具参数定义。已用真实 Codex App Server 验收发现和只读调用，不需要变更你当前的 Codex 配置。

0.1.6 的“验收交付…”提供“确认验收”和“拒绝交付”。打开提示时锁定当前 attempt、revision、补丁及验证收据，后台刷新不会改变确认对象。若交付已被其他客户端更新，窗口提示重新检查。两个决定都不会应用补丁或修改主分支。

0.1.7 为任务窗口加入 macOS 26 液态玻璃状态卡（旧版使用系统半透明材质）、应用图标和系统菜单栏入口。菜单可重新打开窗口、手动刷新或退出界面；退出界面不停止独立的后台服务。后台轮询不再重设已经初始化的选中项，也不在每次刷新前切回连接中。

0.1.8 新增 OpenCode Go + GLM-5.3-Flash 的独立账户验收入口。它能查询套餐额度、核对供应商报告的计费来源；常驻试用 daemon 仍不启用真实路线。安装后可用包内 relayctl 的 `route-pilot go-preview` 查看请求边界。

0.1.9 将侧栏宽度限制在 240–318 pt，详情区优先使用剩余空间。应用前台菜单「任务」与右侧状态栏共用「打开任务窗口」和「刷新服务状态」；系统状态栏图标不可见时仍可从前台菜单或 Dock 打开任务窗口。

0.1.10 增加 Claude Code 受限写入样本。它只写临时受管 worktree，校验最终文件字节和基线完整性，不启动常驻真实任务。

0.1.11 增加双工具账户验收：Claude Code 修改独立合成样本，本机验证，再由 OpenCode 评审同一份差异与收据。可合成响应验收；真实模式仍需账户确认，不向常驻 daemon 提交任务。

0.1.12 接通已登记项目的新建任务窗口和计划确认。真机使用独立的合成仓库及 daemon，点击提交后出现一条待验收任务和一份制品，主 checkout 无变化。默认演示仓库未改为用户项目。

0.1.13 新增图形化本机项目登记，并把默认 Git HEAD 查询切到共享的离线 Git 调用层。登记会生成独立本地资料，不启动对应服务或真实路线。

0.1.14 的「项目」菜单可打开已登记项目并查看独立的本机实例，也可切回离线演示。登记项目暂不派发任务。

0.1.15 增加已登记项目的真实路线预览与显式启动入口。默认项目仍为仅查看。真实模型和计费效果需要用实际账户验收，参见 [本机项目登记](project-registration.md)。

0.1.16 在任务详情保留并显示适配器失败原因，包括套餐额度、认证、限流和计费核验；不会落盘原始供应商响应。

0.1.17 增加实现/评审用量事件，区分 OpenCode Go 的套餐来源与零余额扣费，MiniMax 扣费归属保持 `unverified`。验收事件不含提示词、代码或原始响应。

0.1.18 修复前台 SIGINT/SIGTERM 对活动离线进程组的回收；启动、取消和退出不明继续保留持久派发门。安装健康门、MCP 握手、doctor 和本机档案恢复通过。

0.1.19 增加批次验收修复：重试必须保持原写入范围/规则/执行配置；MCP 业务错误返回结构化 `error`；任务窗口保留断线前状态并支持测试收据/交付说明分页；升级脚本等待 daemon 退出后再取快照；doctor 改为只读打开数据库。本机由 0.1.18 升级到 0.1.19，保留 2 任务/28 事件/2 制品和旧版回退点。

0.1.20 在 0.1.19 基础上完成审计收尾：评审备注落审计事件、状态返回 `next_action`、适配器成功必须有 `worker_started`、制品切片校验 UTF-8 起点、计划默认有效期改为 15 分钟，并补齐开源/架构/安全文档。正式签名、公证、真实双账户和干净用户门仍未完成。

## 维护：配额、清理和日志

以下命令要求先停止本机服务（实例锁会拒绝与运行中的 daemon 并发执行），并只作用于指定的实例目录：

```sh
relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
"$relayctl" maintenance prune --instance-dir "$HOME/.agent-relay-next/instance" --older-than-days 30
# 上面只预览；确认清单后才加 --apply
"$relayctl" maintenance prune --instance-dir "$HOME/.agent-relay-next/instance" --older-than-days 30 --metadata-days 90 --apply
"$relayctl" maintenance rotate-logs --instance-dir "$HOME/.agent-relay-next/instance"
```

- `prune` 只处理已终结且超过保留期的任务的工作树和已登记制品；`--metadata-days 90`（默认）在制品/工作树已清理后再移除任务/事件/阶段审计行，活跃、待验收和不确定任务不处理。默认预览，`--apply` 才删除。
- 受管制品加工作树超过 2 GiB 时，daemon 暂停领取新任务；已运行任务允许完成并保留证据。
- 日志轮转保留最近 5 份、每份上限 5 MiB；安装器的启动包装脚本在服务启动/重启时自动轮转，也可用上面的 maintenance 命令手动执行。

0.1.21 增加生产运维门的基础实现：20 个非终态任务队列上限、受管制品/工作树 2 GiB 配额（领取前采样）、`maintenance prune`（默认预览，`--apply` 清理超期终态 worktree/artifact，保留任务/事件）、`maintenance rotate-logs`（5 MiB × 5，要求服务停止后执行），任务详情显示 StageRun 时间线，关键按钮补可访问性标签。全工作区 230 项测试通过。

0.1.22 优化任务窗口视觉：渐变应用标识、状态胶囊、卡片化服务栏、阶段时间线、更清晰的侧栏和菜单栏浮层；新增 `packaging/release/build-dmg.sh` 与 `verify-dmg-local.sh`，支持无 Apple 开发者账号的未签名 DMG 发布与校验。许可证选择 MIT。未签名 DMG 首次启动仍需手动放行，不能称为 Gatekeeper 通过。

0.1.23 增加 90 天审计元数据清理、启动包装脚本自动日志轮转、版本化模型目录/确定性推荐和更现代的窗口/菜单栏外观；许可证 MIT。未签名 DMG 0.1.23 已构建并通过挂载/SHA/版本校验。全工作区 233 项测试通过。
