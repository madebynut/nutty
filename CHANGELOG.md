# 更新日志

本文件记录 Nutty 每个版本的用户可见变化，格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

发布工作流会把与 `Cargo.toml` 版本号同名的章节作为 GitHub Release 说明，标题格式必须保持为 `## [版本号]`。

## [1.0.0-beta.1]

首个公开测试版。

### 新增

- 设置 → 终端 → 输入新增「终端程序写入剪贴板」：每次询问、允许并通知（默认）、始终允许。询问时显示内容预览，换行、Tab 和控制字符以可见符号标出。
- 端口转发的监听地址不是 `127.0.0.1` 时，在输入框和规则列表中显示警告；启动这类转发会写入日志。
- AI 提供方和 MCP 使用 `http://` 访问非本机地址时显示明文传输警告。
- 日志写入数据目录下的 `logs/nutty.log`（超过 8 MiB 时轮换），崩溃时生成带调用栈的 `crash-*.log`；Windows 启动失败时弹窗并给出日志路径。
- `profile.json` 损坏时自动恢复上一份配置，损坏的文件保留为 `profile-corrupt-*.json`。
- Windows 与无服务端装饰的 Linux 环境（如 GNOME Wayland）提供可拖动的标题栏和窗口按钮。
- Windows Shell 列表发现 WSL 与默认目录下的 Git Bash。
- 发布包附带第三方许可证声明 `THIRD-PARTY-NOTICES.txt`。
- 启动时检查一次 GitHub 上是否有新版本，有则在状态栏和「关于」中提示。只发送一个匿名请求，可在设置 → 常规中关闭。

### 变更

- 导入备份时，新增的 MCP 服务和 ACP Agent 保持停用；替换导入保留本机 Shell，端口转发不再自动启动。
- 推理强度不再在设置的 AI 提供方中逐个模型配置。AI 助手的模型菜单按 Rig 模型目录给出所选模型自己的档位（含能否关闭推理）；目录未收录的模型提供极低到最高的全部档位，默认自动。只按 token 预算思考的模型（如 Claude 4.5、Gemini 2.5）会自动换算成预算。
- 选择支持多档推理强度的模型后，模型菜单保持打开，可以接着选择推理强度。
- macOS Dock 图标按系统图标网格缩小，与其他应用大小一致。
- Linux 窗口设置 Wayland app_id `nutty`，任务栏可匹配桌面项图标；发布包附带 SVG 图标。
- Services、隐藏应用等仅 macOS 有效的菜单项和快捷键不再出现在 Windows 与 Linux。
- 数据目录新建时权限为仅当前用户可访问（Unix）。

### 修复

- SFTP 下载和 Zmodem 接收在 FAT32、exFAT 和部分网络盘上完成后失败。
- Zmodem 在写盘变慢时因缓冲不足中止。
- Linux 上 Secret Service 晚于 Nutty 启动时，需要重启才能访问凭据；现在点击重试即可。
- 通过 IPv6 地址配置的代理无法连接。
- Windows 下 MSYS / Git Bash 设置的 `SHELL` 被当作默认 Shell 导致无法启动。
- 新版本写入的配置会被旧版本静默降级。
- macOS 直接运行时启动日志写入两次。

### 安全

- Kitty 图像协议的文件传输只接受本机绝对路径，拒绝 `/proc`、`/sys`、`/dev` 和 Windows 网络路径，错误回复不再泄露文件是否存在。
- Zmodem 接收的文件名只能是所选目录下的单个普通文件名；Windows 上拒绝盘符、数据流和设备名。
- 限制 Sixel 图像的总绘制量，防止单帧占满 CPU。
- 主机指纹确认只回应用户看到的那一个提示；含 known_hosts 特殊字符的主机名不会被记录。
- 拒绝服务器发起的 SSH Agent 转发通道。
- SFTP 下载拒绝 `CONIN$`、`CONOUT$` 和上标数字的 Windows 设备名。
- russh 固定到已审查的提交；新增 `deny.toml`，CI 和发布前检查安全公告、许可证和依赖来源。
