#!/bin/sh
# Default is the distribution gate. --development checks a local ad-hoc bundle only.
set -eu
task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
development=false
if [ "${1:-}" = --development ]; then development=true; shift; fi
bundle=${1:-"$task_root/target/release/Nutty.app"}
plist="$bundle/Contents/Info.plist"
binary="$bundle/Contents/MacOS/Nutty"
test -x "$binary"
plutil -lint "$plist"
for key in CFBundleName CFBundleDisplayName CFBundleExecutable; do
    test "$(/usr/libexec/PlistBuddy -c "Print :$key" "$plist")" = Nutty
done
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconFile' "$plist")" = Nutty.icns
cmp "$task_root/assets/Nutty.icns" "$bundle/Contents/Resources/Nutty.icns"
codesign --verify --deep --strict "$bundle"
version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist")
test "$("$binary" --version)" = "Nutty $version"
echo "Nutty $version"
if [ "$development" = true ]; then
    echo 'Development bundle identity, artwork and signature verified. This does not approve distribution.'
    exit 0
fi
signature=$(codesign --display --verbose=4 "$bundle" 2>&1)
case "$signature" in
    *"Authority=Developer ID Application:"*) ;;
    *) echo 'Distribution requires a Developer ID Application signature.' >&2; exit 1 ;;
esac
case "$signature" in
    *"runtime"*) ;;
    *) echo 'Distribution requires the hardened runtime.' >&2; exit 1 ;;
esac
xcrun stapler validate "$bundle"
spctl --assess --type execute --verbose=2 "$bundle"
cd "$task_root"
cargo deny --workspace --all-features --target aarch64-apple-darwin check advisories
echo 'macOS distribution checks passed. Environment acceptance and soak testing remain separate requirements.'
