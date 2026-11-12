# Linux 使用与构建

官方包在 Ubuntu 24.04 x86_64 上构建，需要 glibc 2.39 或更新的版本。更早的发行版请从源码构建。快捷键与 Windows 相同，使用 `Ctrl Shift` 组合。

## 运行依赖

解压后直接运行 `./Nutty --version` 不需要图形会话。打开窗口需要 Wayland 或 X11，以及这些运行库：

- `libwayland-client`
- `libxkbcommon`，X11 会话还需要 `libxkbcommon-x11` 和 `libxcb`
- `libvulkan` 以及显卡对应的 Vulkan 驱动（例如 Mesa 的 `mesa-vulkan-drivers`）
- `fontconfig`

Ubuntu/Debian 可执行：

```sh
sudo apt-get install libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libxcb1 libvulkan1 mesa-vulkan-drivers fontconfig
```

凭据走 FreeDesktop Secret Service，不写入 `profile.json`。GNOME Keyring 直接提供该服务；KWallet 需要打开 Secret Service 兼容。没有该服务时，保存或读取凭据会提示钥匙串访问失败，已有终端和本地配置仍可使用。

推荐安装 `fonts-dejavu-core` 与 `fonts-noto-cjk`。缺少这两套字体时，界面会退回系统界面字体。

SSH Agent 优先使用 `SSH_AUTH_SOCK`。从桌面启动、环境变量没有传入时，只在 `XDG_RUNTIME_DIR` 下查找当前用户拥有的套接字，顺序是 `keyring/ssh`、`gcr/ssh`、`ssh-agent.socket`。不扫描 `/tmp`。

把 `Nutty` 放到 `PATH` 后，`nutty.desktop` 才能从应用菜单启动。图标使用同目录的 `nutty.svg`，安装到 `~/.local/share/icons/hicolor/scalable/apps/nutty.svg`，桌面项安装到 `~/.local/share/applications/nutty.desktop`。窗口的 Wayland app_id 为 `nutty`，与桌面项文件名一致，任务栏据此显示图标。

```sh
install -Dm755 Nutty ~/.local/bin/Nutty
install -Dm644 nutty.svg ~/.local/share/icons/hicolor/scalable/apps/nutty.svg
install -Dm644 nutty.desktop ~/.local/share/applications/nutty.desktop
```

GNOME 等不提供服务端窗口装饰的 Wayland 环境中，Nutty 自行绘制标题栏，提供拖动、最小化、最大化和关闭。

## 日志与崩溃报告

日志写入数据目录下的 `logs/nutty.log`（默认 `~/.config/nutty/logs`，或 `NUTTY_DATA_DIR/logs`），超过 8 MiB 时在下次启动轮换为 `nutty.log.1`。程序崩溃时同目录生成 `crash-*.log`，包含调用栈。Nutty 的日志事件不包含密码、密钥或终端内容。设置 `RUST_LOG=debug` 可获得更详细的日志。

## 从源码构建

需要 Rust stable（最低版本见 `Cargo.toml`）、Python 3.11+、C 编译器和 CMake；打包还需要 cargo-about（`cargo install --locked cargo-about --features cli`）。图形库在编译时链接：

```sh
sudo apt-get install pkg-config cmake g++ libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libvulkan-dev libfontconfig1-dev
cargo test --workspace --locked --all-targets --all-features
sh scripts/bundle-linux.sh release
```

CI 在 Ubuntu 24.04 上执行格式检查、Clippy、工作区测试、`--version`，并打包 `.tar.gz`。CI 不打开图形会话，也不代替中文输入法、Wayland 与 X11、显卡、休眠恢复和真实 SSH 服务的桌面验收。
