#!/usr/bin/env bash
# Stable Luma Wayland-session entrypoint. It never builds or uses debug output.
set -euo pipefail

repo_dir=$(cd "$(dirname "$0")/.." && pwd)
release_binary="$repo_dir/target/release/wm"
if [[ ! -x "$release_binary" ]]; then
    echo "Luma stable session requires $release_binary" >&2
    echo 'Build it first with: cargo build --release --workspace --locked --offline' >&2
    exit 1
fi

user_config="${XDG_CONFIG_HOME:-$HOME/.config}/wm/config.toml"
if [[ -f "$user_config" ]]; then
    export WM_CONFIG="${WM_CONFIG:-$user_config}"
else
    export WM_CONFIG="${WM_CONFIG:-$repo_dir/config/default.toml}"
fi
export WM_SOCKET="${WM_SOCKET:-${XDG_RUNTIME_DIR:?}/wm.sock}"
export XDG_CURRENT_DESKTOP=wm:wlr
export RUST_LOG="${RUST_LOG:-wm_compositor=info,smithay=warn}"

exec "$release_binary" --tty-udev
