#!/bin/sh
set -eu

task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$task_root"
if [ ! -f assets/Nutty.icns ] || [ ! -f assets/Nutty.png ] || \
   [ ! -f assets/Nutty-workspace.png ] || \
   [ assets/Nutty.svg -nt assets/Nutty.icns ] || \
   [ scripts/generate-icon.swift -nt assets/Nutty.icns ]; then
    swift scripts/generate-icon.swift
fi
profile=${1:-release}
case "$profile" in
  release) cargo build -p nutty --release --locked ;;
  debug) cargo build -p nutty --locked ;;
  *) echo "usage: $0 [release|debug]" >&2; exit 2 ;;
esac

bundle="$task_root/target/$profile/Nutty.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
# Replace the executable without rewriting the inode used by a running app.
bundle_binary=$(mktemp "$bundle/Contents/MacOS/.Nutty.XXXXXX")
trap 'rm -f "$bundle_binary"' 0 1 2 15
cp -p "$task_root/target/$profile/Nutty" "$bundle_binary"
mv -f "$bundle_binary" "$bundle/Contents/MacOS/Nutty"
cp "$task_root/assets/Nutty.icns" "$bundle/Contents/Resources/Nutty.icns"
cp "$task_root/assets/Info.plist" "$bundle/Contents/Info.plist"
for document in LICENSE CHANGELOG.md SECURITY.md; do
    cp "$task_root/$document" "$bundle/Contents/Resources/$document"
done
sh "$task_root/scripts/third-party-notices.sh" "$bundle/Contents/Resources/THIRD-PARTY-NOTICES.txt"
signing_identity=${NUTTY_SIGNING_IDENTITY:--}
if [ "$signing_identity" = - ]; then
    codesign --force --sign - "$bundle"
    echo 'Nutty uses an ad-hoc signature; rebuilding changes its Keychain authorization identity.' >&2
else
    # Let codesign validate the identity. Never silently fall back on signing failure.
    codesign --force --sign "$signing_identity" --options runtime --timestamp "$bundle"
fi
codesign --verify --deep --strict "$bundle"
echo "$bundle"
