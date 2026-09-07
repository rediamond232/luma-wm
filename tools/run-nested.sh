#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export WM_CONFIG="${WM_CONFIG:-$PWD/config/smoke.toml}"
export WM_SOCKET="${WM_SOCKET:-${XDG_RUNTIME_DIR:?}/wm-nested.sock}"
export XDG_CURRENT_DESKTOP=wm
export WM_PRIVATE_BUS=1
export RUST_BACKTRACE=1
export RUST_LOG="${RUST_LOG:-wm_compositor=info,smithay=warn}"
exec dbus-run-session -- ./target/debug/wm --x11
