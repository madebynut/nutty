<div align="center">
  <img src="assets/Nutty.png" alt="Nutty 坚果终端" width="112" height="112" />
  <h1>Nutty · 坚果终端</h1>
  <p><strong>终端、远程文件与 AI 运维助手，同在一个原生工作区。</strong></p>
  <p>
    <a href="https://github.com/madebynut/nutty/releases"><img src="https://img.shields.io/github/v/release/madebynut/nutty?include_prereleases&style=flat-square&color=52525B&label=release" alt="Latest release" /></a>
    <a href="https://github.com/madebynut/nutty/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/madebynut/nutty/ci.yml?branch=main&style=flat-square&label=CI" alt="CI status" /></a>
    <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-52525B?style=flat-square" alt="macOS, Windows, Linux" />
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-52525B?style=flat-square" alt="Apache License 2.0" /></a>
  </p>
  <p>
    <a href="#下载安装">下载安装</a> ·
    <a href="#功能">功能</a> ·
    <a href="#安全与隐私">安全与隐私</a> ·
    <a href="#从源码构建">从源码构建</a> ·
    <a href="CHANGELOG.md">更新日志</a>
  </p>
</div>

Nutty 是用 Rust 写的桌面终端，面向每天和服务器打交道的人。本地 Shell、SSH 会话、远程文件和 AI 助手放在同一个窗口里，切换时不丢上下文：AI 看得到当前窗格的系统与 Shell，文件面板跟随终端的当前目录，端口转发随连接一起启停。

界面由 [GPUI](https://www.gpui.rs) 在 GPU 上绘制，终端解析使用 [Alacritty](https://github.com/alacritty/alacritty) 的内核，SSH 与 SFTP 基于 [russh](https://github.com/Eugeny/russh)，模型接入使用 [Rig](https://github.com/0xPlaygrounds/rig)。没有 Electron，也没有内嵌浏览器。

> [!NOTE]
> 当前为 **1.0.0 测试版**，界面与配置格式在正式版前仍可能调整。遇到问题欢迎[提交 Issue](https://github.com/madebynut/nutty/issues)。

## 功能

### 终端

- 本地会话支持 zsh、bash、fish、PowerShell、cmd、WSL 与 Git Bash；标签、任意嵌套分屏、跨窗格广播输入和回滚搜索。
- Shell 集成跟踪当前目录与命令边界，命令历史可按窗格查看；可选记录会话日志。
- Kitty 图像协议、Sixel、iTerm 内联图像与 Zmodem 收发。
- 中英文字体分别设置，中文保持双列对齐；行高、字距、光标形状与三套主题可调。
- 重启后恢复标签、分屏和本地目录，SSH 会话保留布局等待重连。

### 连接

- 密码、私钥、OpenSSH 证书、SSH Agent 和键盘交互认证；首次连接核对主机指纹，指纹变化直接拒绝。
- 多级跳板、HTTP CONNECT 与 SOCKS5 代理、保活、断线重连，以及可自定义的密钥交换与加密算法。
- 本地、远程与动态（SOCKS5）端口转发，规则可在连接成功后自动启动。
- 导入 `~/.ssh/config`；配置与凭据可导出为 Argon2id + AES-256-GCM 加密的备份。

### 远程文件

- SFTP 浏览、预览、在线编辑、重命名与权限修改，常用目录可加书签。
- 传输队列支持限速、并发、暂停续传、失败重试和同名冲突处理；下载从不覆盖已有文件。

### AI 助手

- 每个窗格一个独立会话，AI 能看到该窗格的系统、Shell 与当前目录；终端输出只在你打开开关后才作为上下文发送。
- 支持 OpenAI、Anthropic、Google Gemini、DeepSeek、Ollama 以及兼容 OpenAI 接口的服务；模型菜单按 Rig 模型目录给出该模型可用的推理强度。
- 通过 [ACP](https://agentclientprotocol.com) 接入 Claude Code、Codex、Cursor、Gemini CLI 与 OpenCode；支持 MCP 工具与 Skills。
- 建议的命令以卡片呈现，点击才会执行。可选的「只读自动发送」只放行白名单内的查询命令（如 `ls`、`ps`、`uname`），并改写为绝对路径与逐词引用，任何不满足条件的命令都退回手动执行。

## 下载安装

从 [Releases](https://github.com/madebynut/nutty/releases) 下载对应平台的压缩包，并用随附的 `SHA256SUMS.txt` 校验：

```sh
shasum -a 256 -c SHA256SUMS.txt --ignore-missing
```

| 平台 | 文件 | 系统要求 |
| --- | --- | --- |
| macOS · Apple Silicon | `Nutty-<版本>-macos-aarch64.zip` | macOS 13 及以上 |
| macOS · Intel | `Nutty-<版本>-macos-x86_64.zip` | macOS 13 及以上 |
| Windows · x86_64 | `Nutty-<版本>-windows-x86_64.zip` | Windows 10 1809 及以上 |
| Linux · x86_64 | `Nutty-<版本>-linux-x86_64.tar.gz` | glibc 2.39 及以上（如 Ubuntu 24.04），Wayland 或 X11 |

发布包没有经过 Apple 公证或 Windows Authenticode 签名，首次打开时系统会拦一下：

- **macOS**：把 `Nutty.app` 移到「应用程序」后双击；若提示无法验证开发者，到「系统设置 → 隐私与安全性」点「仍要打开」。升级后钥匙串可能再次询问是否允许 Nutty 读取已保存的凭据，选择「始终允许」即可。
- **Windows**：解压后运行 `Nutty.exe`；若出现「Windows 已保护你的电脑」，点「更多信息 → 仍要运行」。
- **Linux**：解压后运行 `./Nutty`。运行库与桌面项安装见包内的 `Linux.md`。

Windows 与 Linux 的完整说明见 [guides/windows.md](guides/windows.md) 与 [guides/linux.md](guides/linux.md)。

## 常用快捷键

| 操作 | macOS | Windows / Linux |
| --- | --- | --- |
| 新建本地终端 | `⌘T` | `Ctrl+Shift+T` |
| 关闭当前窗格 | `⌘W` | `Ctrl+Shift+W` |
| 左右分屏 | `⌘D` | `Ctrl+Shift+D` |
| 上下分屏 | `⌘⇧D` | `Ctrl+Shift+Alt+D` |
| 命令面板 | `⌘K` | `Ctrl+Shift+K` |
| 设置 | `⌘,` | `Ctrl+Shift+,` |
| 键盘快捷键一览 | `⌘/` | `Ctrl+Shift+/` |
| 退出 | `⌘Q` | `Ctrl+Shift+Q` |

## 安全与隐私

Nutty 默认把远端主机的输出当作不可信数据处理：

- **凭据只进系统钥匙串**：macOS 钥匙串、Windows 凭据管理器或 Linux Secret Service。配置文件里没有明文密码与密钥；钥匙串不可用时不会退回明文存储。
- **远端输出受限**：图像协议只能读取本机的普通文件，Zmodem 接收的文件只能落在你选择的目录且不覆盖已有文件，读取剪贴板必须确认，写入剪贴板可设为每次询问。
- **导入不会启动程序**：从备份导入的 MCP 服务与 Agent 默认停用，端口转发不会自动启动。
- **风险操作有提示**：转发监听在 `127.0.0.1` 以外、AI 或 MCP 地址使用明文 `http://` 时，界面会给出警告。
- **网络请求透明**：除你配置的服务器、代理和 AI 服务外，Nutty 只在启动时向 GitHub 查询一次新版本，不附带任何个人信息，可在 设置 → 常规 中关闭。没有遥测，也不上传日志。

日志与崩溃报告只写在本机数据目录的 `logs/` 下。发现安全问题请按 [SECURITY.md](SECURITY.md) 私密报告，不要公开提交 Issue。

## 从源码构建

需要 Rust stable（最低版本见 `Cargo.toml`）与 Python 3.11+。Windows 还需要 Visual Studio 的 C++ 工具与 Windows SDK，Linux 需要图形开发库，详见 [guides/windows.md](guides/windows.md)、[guides/linux.md](guides/linux.md)。

```sh
git clone https://github.com/madebynut/nutty.git
cd nutty
cargo run --locked --bin Nutty
```

提交前的检查与 CI 一致：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo deny --workspace --all-features check
```

打包脚本会生成第三方许可证声明，需要先安装 [cargo-about](https://github.com/EmbarkStudios/cargo-about)：

```sh
cargo install --locked cargo-about --features cli

sh scripts/bundle-macos.sh release     # macOS：target/release/Nutty.app
./scripts/bundle-windows.ps1 release   # Windows：dist/*.zip
sh scripts/bundle-linux.sh release     # Linux：dist/*.tar.gz
```

发布流程、版本号与依赖策略见 [guides/releases.md](guides/releases.md)。

## 项目结构

| 目录 | 内容 |
| --- | --- |
| `src/` | 应用入口与界面：工作区、终端视图、设置、AI 助手 |
| `crates/nutty-core` | 配置、凭据、代理、备份、命令白名单等共享模型 |
| `crates/nutty-terminal` | PTY、终端解析、Shell 集成、图像协议与 Zmodem |
| `crates/nutty-ssh` | SSH 连接、跳板、端口转发、SFTP 与传输队列 |
| `crates/nutty-ai` | 模型调用、ACP Agent、MCP 与 Skills |

## 许可

[Apache License 2.0](LICENSE) · madebynut

Nutty 依赖的开源组件保留各自的许可与著作权，发布包内的 `THIRD-PARTY-NOTICES.txt` 列出了全部组件及其许可证全文。
