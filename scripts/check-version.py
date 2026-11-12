#!/usr/bin/env python3
"""Validate every package/native version and the requested release tag before building."""
import os
import pathlib
import plistlib
import re
import tomllib

root = pathlib.Path(__file__).resolve().parent.parent
manifest = tomllib.loads((root / "Cargo.toml").read_text())
version = manifest["workspace"]["package"]["version"]
lock = tomllib.loads((root / "Cargo.lock").read_text())
semver = re.fullmatch(r"(\d+\.\d+\.\d+)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?", version)
assert semver, "Use a three-part application version, optionally with a pre-release such as -beta.1"
core = semver.group(1)
for package in lock["package"]:
    if package["name"] in ("nutty", "nutty-core", "nutty-terminal", "nutty-ssh", "nutty-ai"):
        assert package["version"] == version, f"Cargo.lock mismatch: {package['name']}"
with (root / "assets/Info.plist").open("rb") as file:
    plist = plistlib.load(file)
# macOS bundle versions must be numeric, so they carry only the numeric part.
assert plist["CFBundleShortVersionString"] == core, "macOS version mismatch"
assert plist["CFBundleVersion"] == core, "macOS build version mismatch"
# Release notes are built from this section, so a missing entry fails before building.
changelog = (root / "CHANGELOG.md").read_text()
assert re.search(rf"^## \[{re.escape(version)}\]", changelog, re.M), f"CHANGELOG.md has no ## [{version}] section"
tag = os.environ.get("NUTTY_RELEASE_TAG", f"v{version}")
assert re.fullmatch(r"v" + re.escape(version) + r"(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?", tag), f"Tag {tag!r} does not match version {version}"
print(f"Nutty {version} · {tag}")
