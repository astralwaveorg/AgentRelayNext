# Agent Relay 0.1.27 — 本机开发预览

**One coding agent writes. Another reviews. You get the evidence.**

未签名开发预览；首次启动需手动放行，不要关闭 Gatekeeper。

## 本版内容

- **自动重试**：上游 429 或瞬时传输失败时，按 30 秒间隔最多重试 10 次
  （`RELAY_RETRY_MAX` / `RELAY_RETRY_INTERVAL_MS`，有上限）；耗尽后任务明确失败，
  不生成制品。
- **额度耗尽备用模型**：`RELAY_REVIEW_FALLBACK_MODEL` 让 OpenCode Go 评审在同一账户内
  切换到备用模型继续；approval 绑定备用模型，用量记录接受主/备任一模型。
- 真实路线工人超时提高到 7 分钟，覆盖有界重试预算。

## 安装与校验

与 0.1.26 相同：下载 DMG 与 sidecar/`SHA256SUMS`，拖到 Applications，首次启动手动放行，
首次运行点击“启用本地预览”。构建：product `0.1.27`，schema `9`。

## 验证摘要

- 本机与 CI 全量测试通过；真实双账户正例复验通过。
- 重试与备用模型由单元测试覆盖；真实额度耗尽无法在不消耗额度的前提下模拟。

## 已知限制

- 未签名、未公证；真实双账户只覆盖受控合成样本；长期额度/限流矩阵未验证。
- 工作副本不是操作系统沙箱。

## 许可证

MIT。
