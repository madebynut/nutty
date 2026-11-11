<div align="center">
  <img src="assets/Nutty.png" alt="Nutty 坚果终端 Logo" width="128" height="128" />
  <h1>Nutty · 坚果终端</h1>
  <p><strong>终端、远程文件与 AI 运维助手，在同一个原生工作区。</strong></p>
  <p>简洁的黑白坚果，装下日常运维的每一步。</p>
  <p>
    <a href="https://github.com/madebynut/nutty/releases"><img src="https://img.shields.io/badge/version-1.0.0-18181B?style=flat-square" alt="Version 1.0.0" /></a>
    <a href="https://github.com/madebynut/nutty/actions/workflows/release.yml"><img src="https://github.com/madebynut/nutty/actions/workflows/release.yml/badge.svg" alt="Release build" /></a>
    <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Windows-52525B?style=flat-square" alt="macOS and Windows" />
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-52525B?style=flat-square" alt="MIT License" /></a>
  </p>
  <p><a href="https://github.com/madebynut/nutty/releases">下载预览版</a> · <a href="#特性">探索特性</a> · <a href="#从源码构建">从源码构建</a> · <a href="guides/releases.md">发布说明</a></p>
</div>

---

Nutty 是使用 Rust 从零构建的桌面终端应用，基于 **GPUI Kit、Alacritty Terminal、russh 和 Rig**。它把本地终端、SSH、SFTP 和 AI 协作放入统一的扁平化界面，让连接、排查、传输与执行保持连贯。

## 为何选择 Nutty

| 原生而轻快 | 以会话为中心 | 把复杂操作变简单 |
| :--- | :--- | :--- |
| Rust 原生应用，GPUI GPU 绘制，无需 Electron/WebView 承载终端。 | 每个分屏独立拥有命令历史、草稿和 AI 会话，切换窗格即可切换上下文。 | 自动发现 Shell、Agent 与模型；路径支持系统文件选择，常用操作直接可见。 |
| Alacritty 负责终端解析，网络与模型请求在后台运行。 | Shell / Agent 模式保留终端可见，面板可以拖动、收起和原位展开。 | 清晰的状态、连接验证、传输队列和失败反馈，降低配置成本。 |

## 特性

### 一个舒适的终端工作区

- **本地 + 远程**：macOS PTY、Windows ConPTY，以及原生 SSH 会话。
- **标签与嵌套分屏**：左右/上下分屏、拖动尺寸、广播、搜索、清屏；重启恢复工作区布局。
- **贴合日常输入**：Tab 交给 Shell 补全；命令发送器支持复杂、多行命令，草稿随窗格保留。
- **目录与历史跟随**：状态栏展示当前完整目录，右栏按窗格显示命令历史；关闭窗格即清理其会话历史。
- **可调的阅读体验**：界面、终端、Agent 的中英文字体独立设置；搜索系统字体，调整行高、字距、内边距、光标与主题。
- **终端内图像**：Kitty、Sixel、iTerm 图像协议及 Zmodem 文件传输。协议子集和桌面兼容性持续完善。

### 认真处理每一次连接与传输

- **完整的常用认证**：密码、私钥/证书、SSH Agent、键盘交互认证与主机指纹确认。
- **复杂网络可配置**：多跳连接、HTTP CONNECT / SOCKS5 代理、保活与重连、本地/远程/动态端口转发。
- **连接管理**：分组、搜索、编辑、删除、独立测试、在线状态；导入 OpenSSH 配置和加密备份。
- **SFTP 文件工作区**：独立连接配置、目录浏览、书签、预览/编辑、新建、重命名、权限与删除。
- **有状态的传输队列**：并发与限速、递归目录、暂停/续传、失败重试、重启恢复，以及同名冲突处理。
- **谨慎写入**：临时文件分阶段传输、发布前校验、常规文件原子替换；JumpServer 目标可绑定独立 SFTP 主机。

### AI 是当前终端的助手

- **多提供方**：OpenAI、DeepSeek、Anthropic、Gemini、Ollama，以及兼容接口；自动获取模型、手动补充、推理选项与连接测试。
- **外部 Agent**：通过 ACP 接入 Cursor、Codex、Claude Code、Gemini CLI、OpenCode 等；发现本地 CLI，按 Agent 实际能力加载模型与配置。
- **贴近运维场景**：提示中包含当前会话的平台/Shell 信息，以资深运维工程师的方式辅助诊断与执行。
- **执行就在代码块内**：点击发送命令，显示已发送状态并允许重发；支持显式启用只读自动发送与终端上下文。
- **会话边界明确**：每窗格独立、可重置；关闭窗格取消对应 AI 请求与 Agent 进程。
- **MCP + Skills**：配置 stdio/HTTP 工具，展示并确认工具调用；从系统文件选择器导入 Skills。

只读自动发送有严格的命令白名单和空提示符检查；受限 Shell 或不满足条件时保留手动执行。第三方 Agent 的模型、认证和功能以其 ACP 实现为准。

## 开始使用

1. 从 [Releases](https://github.com/madebynut/nutty/releases) 下载对应平台的 ZIP，并核对 `SHA256SUMS.txt`。
2. **macOS**：解压得到 `Nutty.app`。**Windows**：解压后运行 `Nutty.exe`。
3. 打开本地终端，或新建 SSH 连接；在设置中选择字体、主题、Shell 和 AI 提供方。
4. 在当前窗格切换 **Shell / Agent**，展开命令发送器或 AI 输入区开始工作。

| 平台 | 架构 | 环境要求 |
| --- | --- | --- |
| macOS | Apple Silicon / Intel | macOS 13+ |
| Windows | x86_64 | Windows 10 1809+ / Windows 11；推荐 Windows 11，详见 [Windows 指南](guides/windows.md) |

预览包尚未完成面向公众分发的签名流程。首次安装提示、Windows Shell 差异、Agent 安装与测试范围请阅读对应指南。

### 常用快捷键

| 操作 | macOS | Windows |
| --- | --- | --- |
| 新建本地终端 | `⌘ T` | `Ctrl Shift T` |
| 关闭当前窗格 | `⌘ W` | `Ctrl Shift W` |
| 左右分屏 | `⌘ D` | `Ctrl Shift D` |
| 上下分屏 | `⌘ Shift D` | `Ctrl Shift Alt D` |
| 命令面板 | `⌘ K` | `Ctrl Shift K` |
| 设置 | `⌘ ,` | `Ctrl Shift ,` |

## 从源码构建

使用 Rust stable，最低版本见 `Cargo.toml`。macOS 需要 Xcode Command Line Tools；Windows 需要 Visual Studio C++ 工具链及 Windows SDK。协议测试使用 Python 3.11+。

```sh
git clone https://github.com/madebynut/nutty.git
cd nutty
cargo run --locked --bin Nutty
```

```sh
# macOS 应用包
sh scripts/bundle-macos.sh release
```

```powershell
# Windows 便携包
./scripts/bundle-windows.ps1 release
```

验证代码：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo doc --workspace --no-deps --locked
```

`NUTTY_DATA_DIR` 可指定隔离配置目录；`Nutty --version`、`Nutty --help` 与 `Nutty --benchmark` 用于版本检查和本地诊断。

## 架构与发布

| 模块 | 职责 |
| --- | --- |
| `nutty` | 原生窗口、布局、输入、设置与渲染 |
| `nutty-core` | 配置、凭据、工作区、平台信息与共享模型 |
| `nutty-terminal` | 终端解析、PTY、Shell 集成、历史与图像协议 |
| `nutty-ssh` | SSH、多跳、端口转发、SFTP 与传输恢复 |
| `nutty-ai` | 提供方、AI 会话、ACP、MCP 与 Skills |

滚动历史、图像缓存、传输队列和模型上下文采用容量限制；后台网络任务支持取消。具体性能仍取决于窗格数量、输出速率、图像、服务器与模型服务。

**CI 只在推送 `v*` tag 时构建**，普通推送和 PR 不构建。构建覆盖 macOS 两种架构和 Windows x86_64，产出应用包与校验清单。临时 tag 发布 Pre-release，正式 tag 经过依赖门禁后生成待审核 Draft。详见 [构建与发布](guides/releases.md)。

## 许可与致谢

[MIT License](LICENSE) · madebynut

感谢 [GPUI Kit](https://github.com/longbridge/gpui-kit)、[Alacritty](https://github.com/alacritty/alacritty)、[russh](https://github.com/Eugeny/russh)、[Rig](https://github.com/0xPlaygrounds/rig) 及其他开源项目。依赖保留各自的许可与著作权。
