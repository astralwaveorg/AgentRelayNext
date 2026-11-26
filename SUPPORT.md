# Support

## 先查阅文档

- [README](README.md) / [中文 README](README.zh-CN.md)：安装、核心功能与架构总览
- [快速上手与运维指南](docs/getting-started.md)：安装、启用服务、升级、回滚、清理与排障
- [真实路线与项目登记](docs/real-routes.md)：项目登记、真实模型启动、重试与备用模型
- [构建与发布流程](docs/release.md)：未签名 DMG 校验与首次启动放行步骤
- [后续开发方案](docs/roadmap.md)：已知限制与后续版本计划

## 提交 Issue 前准备

1. 记录产品版本、构建号、macOS 版本和最小复现步骤。
2. 收集只读诊断输出：

   ```sh
   relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
   "$relayctl" local-preview status
   "$relayctl" doctor --instance-dir "$HOME/.agent-relay-next/instance"
   ```

3. 上传日志前先脱敏。删除所有 API Key、本机用户名路径、私有代码片段和供应商原始响应。不要直接打包上传整个 `~/.agent-relay-next/` 目录。

## 范围说明

- 第三方模型套餐、额度和账单归属以对应供应商控制台为准。
- 当前为未签名开源开发预览，不提供签名公证自动更新或跨平台二进制。
- 安全漏洞按 [SECURITY.md](SECURITY.md) 私下报告，不要在公开 Issue 披露细节。
