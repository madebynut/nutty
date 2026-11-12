#!/usr/bin/env python3
"""Print GitHub release notes: this version's CHANGELOG section plus install instructions."""
import pathlib
import os
import re
import sys
import tomllib

root = pathlib.Path(__file__).resolve().parent.parent
version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
changelog = (root / "CHANGELOG.md").read_text()
match = re.search(
    rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|\Z)", changelog, re.M | re.S
)
if not match or not match.group(1).strip():
    sys.exit(f"CHANGELOG.md has no section for {version}")

repository = os.environ.get("GITHUB_REPOSITORY", "madebynut/nutty")
print(match.group(1).strip())
print(
    f"""

---

## 下载与安装

| 平台 | 文件 |
| --- | --- |
| macOS Apple Silicon | `*-macos-aarch64.zip` |
| macOS Intel | `*-macos-x86_64.zip` |
| Windows x86_64 | `*-windows-x86_64.zip` |
| Linux x86_64（glibc 2.39+） | `*-linux-x86_64.tar.gz` |

下载后用 `SHA256SUMS.txt` 校验：`shasum -a 256 -c SHA256SUMS.txt --ignore-missing`（Windows：`Get-FileHash`）。

发布包没有经过 Apple 公证或 Windows Authenticode 签名，首次打开时系统会提示：

- **macOS**：解压后把 `Nutty.app` 移到「应用程序」，双击后若提示无法验证开发者，到「系统设置 → 隐私与安全性」点「仍要打开」。每次升级后，钥匙串可能再次询问是否允许 Nutty 读取已保存的凭据，选择「始终允许」。
- **Windows**：解压后运行 `Nutty.exe`，若出现「Windows 已保护你的电脑」，点「更多信息 → 仍要运行」。
- **Linux**：解压后运行 `./Nutty`，安装桌面项见包内 `Linux.md`。

每个发布包都附带 `LICENSE` 和第三方许可证声明 `THIRD-PARTY-NOTICES.txt`（macOS 位于 `Nutty.app/Contents/Resources`）。发现安全问题请按 [SECURITY.md](https://github.com/{repository}/blob/main/SECURITY.md) 私密报告，不要公开提交 Issue。"""
)
