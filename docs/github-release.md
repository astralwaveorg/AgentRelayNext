# GitHub 发布（本机工具、无 Apple 开发者账号）

当前发布形态是**未签名、未公证的本机 DMG**。这适合开源项目让用户手动下载试用，但
不能声称通过 Gatekeeper、不能做静默安装或自动更新。正式签名/公证需要 Apple
Developer ID 和 notarytool；没有这些资源时，发布说明必须明确说明首次打开方式。

## 构建

```sh
cd AgentRelayNext
./scripts/check full
rm -rf /private/tmp/relay-release && packaging/package.sh /private/tmp/relay-release
packaging/release/build-dmg.sh \
  /private/tmp/relay-release/RelayTaskWindow.app \
  /private/tmp/relay-release/RelayTaskWindow-0.1.23.dmg
```

`build-dmg.sh` 会在 DMG 内放 App、`Applications` 链接和简短说明，重新挂载校验版本，
并输出 `.sha256` 与 `.manifest.json`。它不会签名、公证或上传。

## 上传前检查

- [ ] `./scripts/check full` 通过，记录版本、build、测试数和源码摘要
- [ ] DMG 的 SHA-256 与 manifest 写入 Release notes
- [ ] DMG 只含 App、Applications 链接和说明，不含任务数据库、日志、凭据或真实样例
- [ ] 在另一台 Mac/干净用户中实际下载并打开；记录 Gatekeeper 提示和手动放行步骤
- [ ] 说明未签名/未公证，不要求用户关闭 Gatekeeper
- [ ] `LICENSE`（MIT）、已知限制、支持矩阵和回退方式在 Release 页面可见
- [ ] 不在 Release 中上传厂商 CLI、账户密钥或未授权资产
- [ ] 有明确发布授权的维护者执行 `gh release create` 或网页上传

## 用户安装说明（可复制）

1. 从 Release 下载 `.dmg` 和 `.sha256`。
2. 在终端核对：`shasum -a 256 RelayTaskWindow-<version>.dmg`，与 Release notes 一致。
3. 打开 DMG，把 `RelayTaskWindow.app` 拖到 `Applications`。
4. 首次启动若被 macOS 阻止：在 Finder 中按住 Control 点击 App → 打开；或到
   “系统设置 → 隐私与安全性”点击“仍要打开”。不要关闭 Gatekeeper。
5. 打开后按 [本机试用](local-preview.md) 检查 doctor；应用不会自动安装 Git 或厂商 CLI。
6. 卸载：退出界面，按 [本机试用](local-preview.md) 停用用户服务，再把 App 移到废纸篓；
   默认保留数据，删除数据需另行确认。

## 版本与资产

Release 至少包含：

```text
RelayTaskWindow-<version>.dmg
RelayTaskWindow-<version>.dmg.sha256
RelayTaskWindow-<version>.dmg.manifest.json
```

可选附 `AgentRelay-rust.spdx.json`（按 [生产包流程](production-package.md) 生成）和
发布说明。不要把内部签名前 ZIP 当作正式 DMG。

## 明确限制

- 未签名/未公证的 DMG 会有 Gatekeeper 警告；这是预期行为，不是安装损坏。
- 用户必须自行安装 Git 和所需 Claude Code/OpenCode CLI；应用不会代装。
- 没有自动更新；升级使用同版本策略和回退点。
- 真实双账户、干净用户、第二 UID、断电和可访问性门仍未完成时，Release 只能标为
  “本机预览/开发预览”，不能标为生产正式版。
