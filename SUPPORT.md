# Support

## 先查这些

- [README](README.md)：构建、本机试用、真实路线和安全边界
- [本机试用](docs/local-preview.md)：安装、升级、回退、数据位置
- [项目登记](docs/project-registration.md)：项目登记和真实路线启动
- [路线核验](docs/route-pilot.md)：Claude Code / OpenCode / MiniMax / OpenCode Go
- [生产包](docs/production-package.md)：签名、公证和 Gatekeeper

## 报告问题前

1. 记录产品版本、build、macOS 版本、实际 CLI 版本和最小复现步骤。
2. 用 `relayctl doctor --instance-dir <dir>` 和 `--dump` 收集只读状态。
3. 脱敏日志：删除凭据、用户目录、任务内容、供应商原始响应和账单。
4. 不要上传 `~/.agent-relay-next/` 整个目录、任务数据库或钥匙串内容。

## 当前不提供

- 真实账户额度、套餐资格或账单归属的客服支持；请在原生产品核对。
- 正式签名/公证包的下载或自动更新；当前只有本机预览。
- 第二 UID、干净用户、断电或正式发行环境的远程操作。
- 通过修改授权、派发门、签名或证据门槛来“让测试通过”。

安全漏洞请按 [SECURITY.md](SECURITY.md) 私下报告，不要开公开 Issue。
