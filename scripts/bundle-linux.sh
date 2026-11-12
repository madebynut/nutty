#!/bin/sh
# Portable Linux archive. Does not modify macOS or Windows bundles.
set -eu

if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo "this bundle is for Linux x86_64" >&2
  exit 1
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
profile=${1:-release}
case "$profile" in
  release) cargo build -p nutty --release --locked ;;
  debug) cargo build -p nutty --locked ;;
  *) echo "usage: $0 [release|debug]" >&2; exit 2 ;;
esac

binary="$root/target/$profile/Nutty"
version=$(python3 -c 'import pathlib, tomllib; print(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["workspace"]["package"]["version"])')
reported=$("$binary" --version)
if [ "$reported" != "Nutty $version" ]; then
  echo "executable version mismatch: $reported" >&2
  exit 1
fi
tag=${NUTTY_RELEASE_TAG:-v$version}
python3 -c 'import re, sys; version, tag = sys.argv[1:]; assert re.fullmatch("v" + re.escape(version) + r"(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?", tag)' "$version" "$tag"

name="Nutty-$tag-linux-x86_64"
stage="$root/target/bundle/$name"
rm -rf "$stage"
mkdir -p "$stage" "$root/dist"
cp -p "$binary" "$stage/Nutty"
cp "$root/assets/nutty.desktop" "$stage/nutty.desktop"
cp "$root/assets/Nutty.png" "$stage/nutty.png"
cp "$root/LICENSE" "$root/README.md" "$stage/"
cp "$root/guides/linux.md" "$stage/Linux.md"
# GNU tar on Ubuntu and bsdtar on macOS both accept -C.
tar -C "$root/target/bundle" -czf "$root/dist/$name.tar.gz" "$name"
echo "$root/dist/$name.tar.gz"
