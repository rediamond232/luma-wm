#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

version="${1:-0.1.0}"
archive="$PWD/dist/luma-wm-$version-x86_64.tar.zst"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT

cargo build --release --workspace --locked --offline
mkdir -p "$PWD/dist"

install -Dm755 target/release/wm "$staging/luma-wm"
install -Dm755 target/release/wmctl "$staging/wmctl"
install -Dm755 target/release/wm-shell-sctk "$staging/wm-shell-sctk"
install -Dm755 packaging/luma-session "$staging/luma-session"
install -Dm644 config/default.toml "$staging/default.toml"
install -Dm644 packaging/luma-wm.desktop "$staging/luma-wm.desktop"
install -Dm644 LICENSE "$staging/LICENSE"
install -Dm644 README.md "$staging/README.md"
install -Dm644 THIRD_PARTY.md "$staging/THIRD_PARTY.md"

tar --sort=name --mtime='UTC 2026-01-01' --owner=0 --group=0 --numeric-owner \
    --zstd -cf "$archive" -C "$staging" .
sha256sum "$archive"
