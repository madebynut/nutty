#!/bin/sh
# Writes the third-party license notices that every release archive must include.
# Usage: sh scripts/third-party-notices.sh OUTPUT
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:?usage: $0 OUTPUT}
if ! command -v cargo-about >/dev/null 2>&1; then
  echo "cargo-about is required: cargo install --locked cargo-about --features cli" >&2
  exit 1
fi
cd "$root"
# --fail stops on any dependency whose license is not accepted in about.toml.
cargo about generate --all-features --fail --output-file "$output" about.hbs
# Guard against an empty or truncated file reaching a release.
test "$(grep -c '^  - ' "$output")" -ge 100
