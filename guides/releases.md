# 构建与发布

应用版本的唯一来源是根 `Cargo.toml` 的 `[workspace.package].version`，当前为 **1.0.0-beta.1**。工作区 crate 继承该版本，根 `Cargo.toml` 中内部 crate 的 `version` 要求需同步修改。Windows PE 资源由 Cargo 生成：字符串版本为完整版本，数字版本为 `1.0.0.0`。macOS 要求包版本只含数字，`Info.plist` 的两个版本字段填写数字部分（当前为 `1.0.0`），`Nutty --version` 显示完整版本。构建脚本和 `scripts/check-version.py` 均会检查一致性。

版本号带预发布后缀（如 `-beta.1`）时，对应 tag 必然含 `-`，发布工作流只会创建 Pre-release，不会产生正式版本。

## 触发规则

推送到 `main`、拉取请求和手动触发运行 `.github/workflows/ci.yml`，分别在 Ubuntu 24.04 和 Windows Server 2025 上完成 Linux x86_64 与 Windows x86_64 的检查、测试和打包。

只有推送 `v*` tag 才运行 `.github/workflows/release.yml`。发布工作流没有定时或手动触发入口。

```sh
# 当前 Beta：tag 与版本一致，发布为预发布。
git tag -a v1.0.0-beta.1 -m "Nutty 1.0.0-beta.1"
git push origin v1.0.0-beta.1

# 同一版本的打包试验：应用内仍显示 1.0.0-beta.1。
git tag -a v1.0.0-beta.1-test.1 -m "Nutty 1.0.0-beta.1 packaging trial"
```

打 tag 前先在 `CHANGELOG.md` 写好与版本号同名的章节（`## [1.0.0-beta.1]`）。`scripts/check-version.py` 在构建前检查它是否存在，发布工作流用 `scripts/release-notes.py` 把这一章节和下载、首次打开说明组成 GitHub Release 说明。

应用启动时通过 GitHub API 读取最近 30 个 Release（`crates/nutty-core/src/update.rs`）：正式版用户只会看到更新的正式版；预发布版用户会看到更新的预发布版和正式版。Draft、带 `test` 的打包试验 tag（如 `v1.0.0-beta.1-test.1`）以及不是 `v<版本号>` 格式的 tag 都不会被提示。

新 tag 必须指向包含完整源码和 workflow 的提交。已经推送的 tag 不覆盖；修复后使用 `test.2`、`test.3` 等新 tag。

## 构建内容

| 产物 | 构建环境 | 内容 |
| --- | --- | --- |
| `Nutty-<tag>-macos-aarch64.zip` | macOS 15 ARM64 | Apple Silicon `Nutty.app` |
| `Nutty-<tag>-macos-x86_64.zip` | macOS 15 Intel | Intel `Nutty.app` |
| `Nutty-<tag>-windows-x86_64.zip` | Windows Server 2025 / MSVC | `Nutty.exe`、许可证、第三方声明、更新日志和使用说明 |
| `Nutty-<tag>-linux-x86_64.tar.gz` | Ubuntu 24.04 x86_64 | `Nutty`、桌面项、图标、许可证、第三方声明、更新日志和使用说明 |

每个平台运行格式、严格 Clippy、工作区测试、Rustdoc，再构建 release。打包检查应用名称、版本、图标和 CLI 启动。每个发布包都附带 `LICENSE`、`CHANGELOG.md`、`SECURITY.md` 和由 cargo-about 生成的 `THIRD-PARTY-NOTICES.txt`（模板 `about.hbs`，接受的许可证见 `about.toml`，须与 `deny.toml` 保持一致）；有依赖的许可证不在列表中时，打包失败。发布任务生成 SHA-256 校验清单；全部平台通过后，临时 tag 创建公开 Pre-release，正式 tag 只创建 Draft，避免未签名包被自动标记为稳定版。

## 发布门禁

依赖策略在根目录 `deny.toml`，覆盖四个发货平台，检查安全公告、许可证、禁用项和依赖来源。CI 在每次推送和拉取请求时运行 `cargo deny --workspace --all-features check`；发布工作流运行同一检查，预发布遇到问题只警告，正式 tag 遇到任何未通过项会停止发布。完整报告见 `dependency-advisories` 附件。

- **安全公告：** 漏洞、unsound、已撤回（yanked）的依赖一律失败。停止维护的依赖默认失败，只有 `deny.toml` 中逐条写明理由和移除条件的才豁免，目前 7 条，均为 GPUI/gpui-kit 和 zbus 的间接依赖，没有已知漏洞。
- **许可证：** 只允许与 Apache-2.0 分发兼容的宽松许可证；MPL-2.0 仅对 `option-ext`、`dwrote`、`cbindgen` 三个未修改的依赖例外。
- **来源：** 只允许 crates.io 和 `github.com/Eugeny/russh`（在 `Cargo.toml` 中固定提交）。

升级 GPUI、gpui-kit、keyring 或 zbus 后，重新运行检查：不再出现的豁免会提示 `advisory-not-detected`，应从 `deny.toml` 删除。预发布成功仅证明这次自动检查和打包通过。

macOS 构建默认为 ad-hoc 签名；本机可设置 `NUTTY_SIGNING_IDENTITY` 使用 Developer ID，然后完成 notarization/stapling 并运行 `scripts/check-release-macos.sh`。Windows 包未作 Authenticode 签名，Linux 官方包不签名。CI 未存放或生成开发者证书，当前也不自动公证。作为开源项目，未签名的包可以发布：Release 说明会告诉用户如何在 macOS 和 Windows 上首次打开。

发布负责人还需在 Windows/macOS 真机验证中文输入法、DPI/显卡、休眠恢复、真实 SSH/JumpServer/SFTP/AI 服务及长时间负载。CI 成功和版本号均不代表这些验收已通过。
