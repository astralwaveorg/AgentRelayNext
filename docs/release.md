# 构建、校验与发布流程

Agent Relay 目前通过 GitHub Releases 发布未签名的 macOS arm64 预览包（`prerelease: true`）。因为没有 Apple Developer ID 账号，发布产物不含签名、公证或 Gatekeeper 自动放行。

## 1. 本地检查与打包

依次运行检查、打包和 DMG 构建：

```sh
# 1. 格式、Clippy 与全量工作区测试
./scripts/check full

# 2. 构建 release 二进制、SwiftUI .app 和稳定命名 ZIP
packaging/package.sh "$PWD/dist"

# 3. 构建未签名 DMG、.sha256 和 .manifest.json
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' crates/relay-daemon/Cargo.toml)"
packaging/release/build-dmg.sh \
  "$PWD/dist/RelayTaskWindow.app" \
  "$PWD/dist/RelayTaskWindow-$version.dmg"
```

生成 Cargo 依赖 SBOM 并校验全部发布资产：

```sh
# 生成 Rust 依赖 SPDX SBOM（仅含运行与构建依赖，不含测试依赖）
cargo metadata --format-version 1 --filter-platform aarch64-apple-darwin \
  --locked --offline | /usr/bin/python3 -I packaging/release/rust-sbom.py \
  > "$PWD/dist/AgentRelay-rust.spdx.json"

# 准备发布说明并生成、回检 SHA256SUMS
cp CHANGELOG.md "$PWD/dist/RELEASE_NOTES.md"
python3 packaging/release/verify-assets.py "$PWD/dist"
```

`verify-assets.py` 检查同版本只存在一套 ZIP 和一套 DMG，确认每个主资产都有匹配的 `.sha256` sidecar 和 `.manifest.json`，再写入并回检 `SHA256SUMS`。

## 2. 原生 arm64 冒烟与升级演练

在独立临时 HOME 下跑端到端冒烟，不触碰本机安装：

```sh
packaging/release/smoke-native.sh \
  "$PWD/dist/RelayTaskWindow.app" \
  "$PWD/dist/RelayTaskWindow-$version.dmg"
```

该脚本依次验证 `version`、`local-preview prepare`、前台 daemon 启动、`doctor`、`mcp-verify`、离线 demo 提交与验收、制品摘要一致性、主 checkout 无变化，以及 DMG 只读挂载和 `RelayTaskWindow --dump`。

验证跨版本升级与失败回滚：

```sh
packaging/rehearse-upgrade.sh <旧版本.zip> <新版本.zip>
packaging/rehearse-rollback.sh <版本.zip>
```

演练使用独立的临时 `HOME` 和 LaunchAgent 标签，先用故意启动失败的候选包验证自动回滚，再验证正常升级、任务历史保留和维护模式拒写。

## 3. GitHub Release 自动化发布

[`.github/workflows/release.yml`](../.github/workflows/release.yml) 仅在推送 `v*` tag 时触发：

1. 核对 tag 版本与 `crates/relay-daemon/Cargo.toml` 完全一致。
2. 在 `macos-26` arm64 runner 上运行 `./scripts/check full`。
3. 生成稳定命名资产（不带构建时间戳，避免同版本出现多套 ZIP）：
   - `RelayTaskWindow-<version>-macos-arm64.zip`、`.sha256` 和 `.manifest.json`
   - `RelayTaskWindow-<version>.dmg`、`.sha256` 和 `.manifest.json`
   - `AgentRelay-rust.spdx.json`
   - `RELEASE_NOTES.md`（存在 `docs/release-notes-<version>.md` 时优先使用，否则自动使用 `CHANGELOG.md`）
   - `SHA256SUMS`
4. 运行 `verify-assets.py` 和 `smoke-native.sh` 门禁。
5. 以 `prerelease: true`、`overwrite_files: false` 和 `fail_on_unmatched_files: true` 发布 GitHub Release。

### 发布前检查清单

- [ ] `crates/relay-daemon/Cargo.toml`、`Cargo.lock`、`CHANGELOG.md` 和文档版本一致
- [ ] 工作区无未跟踪内部文件、无密钥、无本机绝对路径
- [ ] `./scripts/check full` 和 `smoke-native.sh` 在本机与 CI `main` 均已通过
- [ ] 本机已通过 `packaging/upgrade-local-preview.sh` 升级且 `doctor` 和 `mcp-verify` 通过
- [ ] 在最终提交上打不可变 annotated tag：`git tag -a v<version> -m "Agent Relay <version>" && git push origin v<version>`
- [ ] Release 上线后下载全部附件到临时目录，运行 `shasum -a 256 -c SHA256SUMS` 全量通过，并挂载 DMG 核对 `relayctl version`

## 4. 未来签名与公证预留

如果未来引入 Apple Developer ID 和 `notarytool`，需要在受控发行机移除预览包内签名前的 `SHA256` 文件，分别对 `relayctl` 和外层 `.app` 启用 `--options runtime --timestamp` 签名（不要使用 `--deep`），公证并附加票据后用 `packaging/release/verify-dmg.sh` 复验。
