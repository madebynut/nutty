# Linux 使用与构建

官方包在 Ubuntu 24.04 x86_64 上构建，需要 glibc 2.39 或更新的版本。更早的发行版请从源码构建。快捷键与 Windows 相同，使用 `Ctrl Shift` 组合。

## 运行依赖

解压后直接运行 `./Nutty --version` 不需要图形会话。打开窗口需要 Wayland 或 X11，以及这些运行库：

- `libwayland-client`
- `libxkbcommon`
- `libvulkan`
- `fontconfig`

凭据走 FreeDesktop Secret Service，不写入 `profile.json`。GNOME Keyring 直接提供该服务；KWallet 需要打开 Secret Service 兼容。没有该服务时，保存或读取凭据会提示钥匙串访问失败，已有终端和本地配置仍可使用。

推荐安装 `fonts-dejavu-core` 与 `fonts-noto-cjk`。缺少这两套字体时，界面会退回系统界面字体。

SSH Agent 优先使用 `SSH_AUTH_SOCK`。从桌面启动、环境变量没有传入时，只在 `XDG_RUNTIME_DIR` 下查找当前用户拥有的套接字，顺序是 `keyring/ssh`、`gcr/ssh`、`ssh-agent.socket`。不扫描 `/tmp`。

把 `Nutty` 放到 `PATH` 后，`nutty.desktop` 才能从应用菜单启动。图标使用同目录的 `nutty.png`，安装到 `~/.local/share/icons/hicolor/128x128/apps/nutty.png`，桌面项安装到 `~/.local/share/applications/nutty.desktop`。

## 从源码构建

需要 Rust stable（最低版本见 `Cargo.toml`）、Python 3.11+、C 编译器和 CMake。图形库在编译时链接：

```sh
sudo apt-get install pkg-config cmake g++ libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libvulkan-dev libfontconfig1-dev
cargo test --workspace --locked --all-targets --all-features
sh scripts/bundle-linux.sh release
```

CI 在 Ubuntu 24.04 上执行格式检查、Clippy、工作区测试、`--version`，并打包 `.tar.gz`。CI 不打开图形会话，也不代替中文输入法、Wayland 与 X11、显卡、休眠恢复和真实 SSH 服务的桌面验收。
