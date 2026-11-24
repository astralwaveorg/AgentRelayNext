# macOS 正式包：准备与验收

当前已安装的 0.1.23 是本机试用包（历史文档中的 0.1.9 是旧记录）。没有 Apple 开发者账号时，只能按 [GitHub 未签名 DMG](github-release.md) 发布开发预览，不能执行下面的签名/公证正式流程。它可以演练安装、升级和回退。正式包须由发行负责人签名、公证，核实账户和工具的集成授权，并完成真实路线及干净用户验收。以下命令不会把本机试用版升级为正式版。

## 在开发机准备依赖清单

```sh
set -o pipefail
cargo metadata --format-version 1 --filter-platform aarch64-apple-darwin --locked --offline |
  /usr/bin/python3 -I packaging/release/rust-sbom.py > AgentRelay-rust.spdx.json
```

清单从锁定的 Cargo 构建图生成，包含 relayctl 的运行依赖和构建依赖，排除仅用于测试的依赖。它不涵盖 Swift 工具链、系统框架、App 资源或 crate 内嵌的第三方源代码。在正式发布前，另记录实际工具链和打包文件，核对 bundled SQLite、许可证原文、图标与脚本等文件的分发权；`NOASSERTION` 不代表已获授权。正式资产另附版本说明、支持范围、签名后的 SHA-256、构建来源及 QA 摘要。

## 在受控发行机制作

1. 固定经审查的源码和 Cargo.lock，构建发布候选。用新目录暂存应用；不要修改已安装的预览。
2. 暂存副本移除两个预览版内部 SHA 文件（`Contents/MacOS/SHA256` 与 `Contents/Resources/relay/SHA256`），它们对应签名前的二进制；正式包由代码签名和外部 DMG 摘要验证，不走预览用 ZIP 安装器。
3. 用发行负责人的 Developer ID Application 签名。先签 `Contents/Resources/relay/relayctl`（`--options runtime --timestamp`），再签外层 App（同样使用 Hardened Runtime 和时间戳）。不要用 `--deep` 签名。验证两处签名后制作并签名 DMG。
4. 负责人将签名 DMG 提交给 Apple 公证，确认状态 `Accepted` 后附加票据。以附加票据后的最终 DMG 生成 SHA-256。核查未改动第三方 CLI 的认证或分发方式；不要将供应商二进制加入包。上传和发布分别需要负责人放行。

向 Apple 提交使用受控机器的 notarytool keychain profile。不要把证书、公证凭据、供应商密钥或原始账单存入仓库、包、日志或交接材料。

## 从实际下载入口复验

```sh
packaging/release/verify-dmg.sh <本机下载的DMG> <可信渠道取得的SHA256> <预先确认的Team-ID>
```

脚本先核对下载摘要，再核对 stapled 公证票据、Gatekeeper、DMG 和 App/relayctl 的 Developer ID；只读挂载，不运行包内程序。App 和 relayctl 还要求 Hardened Runtime 和可信时间戳。摘要和 Team ID 都必须来自预先核对的发行记录。通过后，再在独立用户会话中使用正常下载隔离流程试装，检查后台服务、任务窗口、MCP、升级和回退。

本机代码签名钥匙串暂无有效身份。当前已验证 Cargo 依赖清单生成及错误摘要拒绝；签名 DMG 正例、正常下载路径和干净用户安装仍待实测。
