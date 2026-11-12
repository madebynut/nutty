# 构建与发布

应用版本的唯一来源是根 `Cargo.toml` 的 `[workspace.package].version`，当前为 **1.0.0**。工作区 crate 继承该版本，Windows PE 资源由 Cargo 生成。macOS `Info.plist` 的两个版本字段同步更新，构建脚本和 `scripts/check-version.py` 均会检查一致性。

## 触发规则

推送到 `main`、拉取请求和手动触发运行 `.github/workflows/ci.yml`，在 Ubuntu 24.04 上完成 Linux x86_64 的检查、测试和打包。

只有推送 `v*` tag 才运行 `.github/workflows/release.yml`。发布工作流没有定时或手动触发入口。

```sh
# 临时测试：应用内仍显示 1.0.0，分发 tag 标记为预发布。
git tag -a v1.0.0-test.1 -m "Nutty 1.0.0 packaging trial"
git push origin v1.0.0-test.1
```

新 tag 必须指向包含完整源码和 workflow 的提交。已经推送的 tag 不覆盖；修复后使用 `test.2`、`test.3` 等新 tag。

## 构建内容

| 产物 | 构建环境 | 内容 |
| --- | --- | --- |
| `Nutty-<tag>-macos-aarch64.zip` | macOS 15 ARM64 | Apple Silicon `Nutty.app` |
| `Nutty-<tag>-macos-x86_64.zip` | macOS 15 Intel | Intel `Nutty.app` |
| `Nutty-<tag>-windows-x86_64.zip` | Windows Server 2025 / MSVC | `Nutty.exe`、许可和使用说明 |
| `Nutty-<tag>-linux-x86_64.tar.gz` | Ubuntu 24.04 x86_64 | `Nutty`、桌面项、图标、许可和使用说明 |

每个平台运行格式、严格 Clippy、工作区测试、Rustdoc，再构建 release。打包检查应用名称、版本、图标和 CLI 启动。发布任务生成 SHA-256 校验清单；全部平台通过后，临时 tag 创建公开 Pre-release，正式 tag 只创建 Draft，避免未签名包被自动标记为稳定版。

## 发布门禁

`cargo deny` 审核所有发货平台的依赖。预发布会保留完整审计报告，并在命中或审计失败时明确警告；正式 tag 遇到任何未通过的审计会停止发布。没有设置静默忽略列表。

当前依赖链仍有前次审计发现的 RSA 公告匹配及停维依赖，需以每次 CI 的 `dependency-advisories` 附件为准。预发布成功仅证明这次自动检查和打包通过。

macOS 构建默认为 ad-hoc 签名；本机可设置 `NUTTY_SIGNING_IDENTITY` 使用 Developer ID，然后完成 notarization/stapling 并运行 `scripts/check-release-macos.sh`。Windows 需在正式分发前完成 Authenticode 签名。Linux 官方包不签名。CI 未存放或生成开发者证书，当前也不自动公证。

发布负责人还需在 Windows/macOS 真机验证中文输入法、DPI/显卡、休眠恢复、真实 SSH/JumpServer/SFTP/AI 服务及长时间负载。CI 成功和版本号均不代表这些验收已通过。
