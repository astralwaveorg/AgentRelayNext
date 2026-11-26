# Documentation

[English README](../README.md) · [简体中文 README](../README.zh-CN.md)

## 使用与运维

- [快速上手与运维指南](getting-started.md)：安装、`local-preview enable|status|disable`、离线演示、升级回滚、磁盘配额、清理与日志轮转。
- [真实路线与项目登记](real-routes.md)：登记本地 Git 仓库、真实双账户路线预览与启动、模型目录、自动重试与备用模型、隐私与计费边界。

## 架构与发布

- [架构与安全边界](architecture.md)：6 个 Rust crate 和 SwiftUI UI 的模块分工、状态机、评审链与安全不变量。
- [构建与发布流程](release.md)：未签名 DMG 打包、SBOM、`verify-assets.py`、`smoke-native.sh` 与 GitHub Release 流程。
- [真实运行脱敏证据](evidence/README.md)：真实双账户合成验收的脱敏 JSON 证据包与复现方法。

## 演进规划

- [后续开发方案](roadmap.md)：市场定位分析、现状诊断与 `v0.1.28` 到 `v1.0` 分阶段方案。

## 项目治理

- [Changelog](../CHANGELOG.md) · [Contributing](../CONTRIBUTING.md) · [Security](../SECURITY.md) · [Support](../SUPPORT.md) · [License](../LICENSE)
