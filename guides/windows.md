# Windows 使用与构建

Nutty 的 Windows 版本使用原生 GPUI/Direct3D 窗口、ConPTY 和 Windows 凭据管理器。

## 安装

支持 Windows 10 1809 及以上、Windows 11（x86_64）。建议使用仍在微软支持周期内的 Windows 11，并更新显卡驱动。解压 Windows ZIP 后运行 `Nutty.exe`，无需管理员权限。测试版本未作 Authenticode 签名，尚未建立 SmartScreen 信誉。

默认本地终端为 Windows PowerShell。设置中可选择已安装的 PowerShell 7、cmd.exe 等 Shell；PowerShell 自动保留用户 profile/提示符，并发送目录与命令边界。cmd.exe 启动时发送目录和命令边界；自定义 `PROMPT` 只在不含 `%&|<>^"` 等特殊字符时保留，否则使用 `$P$G`。

本地 Windows 会话会向 AI 传递 Windows 和当前 Shell 信息；远程 SSH 会话根据远端系统生成指令。Windows 本地的 AI 命令支持点击执行。PowerShell 的只读自动发送使用独立 cmdlet 白名单，并改写成模块限定名；cmd 不自动发送。POSIX 白名单不会套用到 PowerShell。

## SSH、Agent 与文件

- 密码、私钥/证书、键盘交互认证、多跳、HTTP CONNECT/SOCKS5 与 SFTP 使用相同 Rust 核心。
- OpenSSH Agent 默认连接 `\\.\pipe\openssh-ssh-agent`。在 Windows 中启用 OpenSSH Authentication Agent 服务并用 `ssh-add` 加载密钥，随后在连接页测试 Agent。
- ACP/MCP 可发现 PATH、用户 npm、WinGet 和 Node.js 安装目录。npm `.cmd` 启动器使用 Rust 标准库的 Windows 参数处理；ACP 用 Job Object 回收后代进程。第三方 CLI 仍须自行安装和登录。
- 本地保存路径支持盘符、中文、空格；下载文件名会拒绝 Windows 保留名称及非法字符。创建符号链接取决于 Windows 开发者模式/权限，当前下载保留链接的任务会明确报错，可选择跳过链接。
- 配置位于 Windows 用户配置目录中的 `nutty` 路径（以应用设置显示为准）；`NUTTY_DATA_DIR` 可指定隔离测试目录。凭据由系统凭据管理器保存，配置文件不包含明文密码。

## 从源码构建

安装 Rust stable（MSRV 见 `Cargo.toml`）、Visual Studio 2022/2026 的“使用 C++ 的桌面开发”和 Windows 10/11 SDK。发布版 GPUI 用 SDK 的 `fxc.exe` 编译着色器，脚本会使用其自动探测；必要时指定 `GPUI_FXC_PATH`。测试需要 Python 3.11+（`python` 在 PATH 中）。

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
./scripts/bundle-windows.ps1 release
```

CI 同时验证 ConPTY 输入/输出、PowerShell 目录与命令历史、窗格隔离、协议会话和传输任务。CI 不替代中文输入法、多显示器/DPI、显卡、睡眠恢复和真实服务器的桌面验收。
