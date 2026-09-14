#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

version="${1:-0.1.0}"
archive="$PWD/dist/luma-wm-$version-x86_64.tar.zst"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT

cargo build --release --workspace --locked --offline
make -C native/game-capture-gl all
make -C native/game-capture-inject all
make -C native/vulkan-external-capture-prototype all
mkdir -p "$PWD/dist"

install -Dm755 target/release/wm "$staging/luma-wm"
install -Dm755 target/release/wmctl "$staging/wmctl"
install -Dm755 target/release/luma-recorder-engine "$staging/luma-recorder-engine"
install -Dm755 target/release/luma-game-capture-muxer "$staging/luma-game-capture-muxer"
install -Dm755 target/release/wm-shell-sctk "$staging/wm-shell-sctk"
install -Dm755 native/game-capture-gl/luma-game-capture-gl "$staging/luma-game-capture-gl"
install -Dm755 native/game-capture-gl/luma-game-record-gl "$staging/luma-game-record-gl"
install -Dm755 native/game-capture-gl/luma-game-attach "$staging/luma-game-attach"
install -Dm755 native/game-capture-inject/build/luma-game-inject "$staging/luma-game-inject"
install -Dm755 native/vulkan-external-capture-prototype/luma-game-record-vulkan \
    "$staging/luma-game-record-vulkan"
install -Dm755 native/vulkan-external-capture-prototype/luma-vulkan-capture-receiver \
    "$staging/luma-vulkan-capture-receiver"
install -Dm755 native/vulkan-external-capture-prototype/luma-vulkan-game-launch \
    "$staging/luma-vulkan-game-launch"
install -Dm755 native/vulkan-external-capture-prototype/libluma_vk_dmabuf_capture.so \
    "$staging/libluma_vk_dmabuf_capture.so"
install -Dm755 native/game-capture-gl/build/libluma-game-capture-gl.so \
    "$staging/libluma-game-capture-gl.so"
install -Dm755 packaging/luma-session "$staging/luma-session"
install -Dm644 config/default.toml "$staging/default.toml"
install -Dm644 packaging/luma-wm.desktop "$staging/luma-wm.desktop"
install -Dm644 packaging/luma-recorder.desktop "$staging/luma-recorder.desktop"
install -Dm644 LICENSE "$staging/LICENSE"
install -Dm644 README.md "$staging/README.md"
install -Dm644 THIRD_PARTY.md "$staging/THIRD_PARTY.md"

tar --sort=name --mtime='UTC 2026-01-01' --owner=0 --group=0 --numeric-owner \
    --zstd -cf "$archive" -C "$staging" .
sha256sum "$archive"
