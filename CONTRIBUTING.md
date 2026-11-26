# Contributing

提交代码或文档前，请先阅读 [README](README.md)、[架构与安全边界](docs/architecture.md) 和 [后续开发方案](docs/roadmap.md)。

## 开发流程

1. 从 `main` 切出短分支。每个提交只处理一个可独立审查的目标。
2. 改动前确认影响范围，不要触碰用户本地数据、配置和安装备份。
3. 先对受影响的 crate 运行定向测试：

   ```sh
   ./scripts/check test relay-git
   ./scripts/check test relay-daemon
   ```

4. 提交前运行完整本地检查：

   ```sh
   ./scripts/check full
   ```

   该命令依次运行 `cargo fmt --all --check`、全工作区 `clippy -D warnings` 和全工作区测试。不要无故更新 `Cargo.lock` 或跳过失败项。

5. 如果修改了 SwiftUI 代码，另编译一次 `.app` 验证：

   ```sh
   tmp=$(mktemp -d /private/tmp/relay-ui.XXXXXX)
   packaging/build-app.sh "$tmp/RelayTaskWindow.app" 0.1.0 dev
   rm -rf "$tmp"
   ```

## 安全与隐私要求

- 不要提交真实凭据、API Key、任务数据库、原始 CLI 会话、个人绝对路径或账单文件。
- 自动化测试必须使用临时目录和离线替身，不能在测试中发起真实模型请求。
- 不要为了通过测试而放宽写入白名单、派发门、进程组回收或制品摘要校验。
- 安全问题按 [SECURITY.md](SECURITY.md) 私下报告，不要开公开 Issue。

## 代码与提交规范

- 保持各 crate 边界清晰（`relay-core`、`relay-protocol`、`relay-store`、`relay-git`、`relay-daemon`、`relay-sys`）。
- 业务错误返回稳定的 `code` 和 `action`。原始供应商载荷、提示词和凭据不能写入日志或事件表。
- 新增功能附带回归测试。修复缺陷先写复现测试再改代码。
- 推荐提交信息格式：

  ```text
  <area>: <imperative summary>
  ```

  常用 `area`：`core`、`store`、`git`、`protocol`、`daemon`、`ui`、`packaging`、`docs`。

## 许可证

本项目采用 [MIT License](LICENSE)。提交贡献表示你同意按 MIT 许可证分发改动。
