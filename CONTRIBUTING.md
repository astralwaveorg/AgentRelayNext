# Contributing

感谢参与 Agent Relay。当前仓库是开发中的本机预览，提交前请先读
[README](README.md)、[架构](docs/architecture.md) 和
[发布说明](docs/release-notes-0.1.23.md)。

## 开发流程

1. 从 `main` 创建短分支；一个提交只处理一个可审查的目标。
2. 改动前确认允许编辑的路径，保留用户配置、任务数据、日志和安装备份。
3. 受影响 crate 先跑定向测试：

   ```sh
   ./scripts/check test relay-git
   ./scripts/check test relay-daemon
   ```

4. 交付前跑完整门：

   ```sh
   ./scripts/check full
   ```

   `full` 依次执行 `cargo fmt --all --check`、全工作区 Clippy（警告视为错误）和
   全工作区测试。不要自动联网、更新锁文件或跳过失败项。

5. 改 UI 时另编译一次：

   ```sh
   tmp=$(mktemp -d /private/tmp/relay-ui.XXXXXX)
   packaging/build-app.sh "$tmp/RelayTaskWindow.app" 0.1.0 dev
   rm -rf "$tmp"
   ```

## 安全与隐私

- 不要提交真实凭据、钥匙串内容、任务数据库、原始 CLI 会话、用户路径或账单。
- 测试使用临时 HOME/仓库/实例和合成供应商；不要为测试消耗真实套餐。
- 不要降低授权、派发门、进程回收、制品完整性、签名或证据门槛来让测试通过。
- 安全问题不要开公开 Issue，按 [SECURITY.md](SECURITY.md) 私下报告。

## 代码风格

- 保持现有模块边界：协议、状态机、存储、Git、daemon、UI 不互相越权。
- 错误返回稳定 code/action；不把原始供应商 payload、提示词或凭据写进日志/事件。
- 新增行为要有可复现测试；修复缺陷先固定失败断言，再修复，再保留首次失败证据。
- 文档、注释和用户可见文案用清晰中文或英文；不要留下内部里程碑缩写作为产品描述。

## 提交信息

建议格式：

```text
<area>: <imperative summary>

Why:
- ...

What changed:
- ...

Verification:
- command and result
```

`area` 可用 `core`、`store`、`git`、`protocol`、`daemon`、`ui`、`packaging`、`docs`。

## 许可证

项目采用 [MIT License](LICENSE)。提交贡献即表示你同意按该许可证分发；不要提交你
无权授权的第三方代码、资产或凭据。
