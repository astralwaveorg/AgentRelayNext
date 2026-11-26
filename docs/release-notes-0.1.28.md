# Agent Relay 0.1.28（本机开发预览）

一个编码 Agent 写代码，另一个独立评审，把可核验的证据交给你。

未签名开发预览。首次启动需要手动放行，不要全局关闭 Gatekeeper。

## 本版更新

- 进程组归属复核（A11）：发送 `SIGTERM` 或 `SIGKILL` 前通过内核接口核对 `leader_pid` 与启动时间令牌，对伪造或已复用的 `pgid` 拒绝发信号。
- 派发门终态收敛（A10）：在 `register_artifact_in_tx` 同事务内统一释放 `dispatch_gate`，统一 `review_active` 与 `implementation_only` 路径。
- 实现模型可配置与回退：支持 `RELAY_IMPLEMENT_MODEL` 与 `RELAY_IMPLEMENT_FALLBACK_MODEL`，贯穿请求体、批准指纹与用量核验。
- 工程加固汇总：统一模型身份归一化（A13）、补全验证输出凭据过滤（A15）、失败原因有界落库（A16）、`relay.sock`/`.own` 权限 `0600`（A18）、SQLite 全表全列校验（A19）、`prune` 回收临时目录（A12）、`ui.overview` 单评审载荷裁剪（A14）、运行期 copytruncate 日志轮转（A17）与停服后再置维护标记的升级握手（A20）。
- UI 状态投影测试：抽离纯逻辑 `MenuStatus.swift`，将 `swiftc` 断言测试接入 `./scripts/check full`。
- 开源文档整理：精简并更新中英文 README 与 `docs/` 指南，新增 `docs/roadmap.md`。

## 安装与校验

下载 DMG、`SHA256SUMS` 与对应 sidecar 文件，核对摘要后把 `RelayTaskWindow.app` 拖入 `Applications`：

```sh
shasum -a 256 -c SHA256SUMS --ignore-missing
```

首次启动手动放行后点击 **启用本地预览**。版本信息：product `0.1.28`，wire `1`，DB schema `9`。

## 验证摘要

- `./scripts/check full` 全工作区测试与 Swift UI 状态测试通过，原生 arm64 smoke 和跨版本升级回滚演练通过。
- 真实双账户合成正例通过：MiniMax CN `MiniMax-M3.1-Flash` 实现 + OpenCode Go `deepseek-v4.1-flash` 评审（`balance_charge_usd=0`，来源 `go`）。
- B4 四项真实负例全部通过：认证失败（`UPSTREAM_AUTH`，1.66 s 无重试无制品）、已完成任务幂等重放（返回同一 task 且零新增调用）、运行中取消（进程组回收且无制品）与停机重启恢复（归入 `interrupted` 不重复派发）。脱敏证据见 `docs/evidence/`。

## 已知限制

- 未签名、未公证，仅限 macOS 14+ Apple Silicon 本机开发预览。
- 当前真实路线仍面向受控双账户合成与单轮编辑。开放 BYOK、多 CLI 与多轮编码计划在 `v0.2` 和 `v0.3` 落地，见 `docs/roadmap.md`。

## 许可证

MIT。
