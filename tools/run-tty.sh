#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $(id -u) == 0 ]]; then echo 'Run from your normal user login on TTY3, without sudo.' >&2; exit 1; fi
if [[ $(tty) != /dev/tty3 ]]; then echo 'Log in on TTY3 and run this command there.' >&2; exit 1; fi
[[ -x target/debug/wm ]] || { echo 'Run ./tools/build-dev.sh first.' >&2; exit 1; }
user_config="${XDG_CONFIG_HOME:-$HOME/.config}/wm/config.toml"
if [[ -f "$user_config" ]]; then
    export WM_CONFIG="${WM_CONFIG:-$user_config}"
else
    export WM_CONFIG="${WM_CONFIG:-$PWD/config/default.toml}"
fi
export WM_SOCKET="${XDG_RUNTIME_DIR:?Log in through a normal system session}/wm.sock"
export RUST_BACKTRACE=1
export XDG_CURRENT_DESKTOP=wm
export WM_PRIVATE_BUS=1
export RUST_LOG="${RUST_LOG:-wm_compositor=info,smithay=warn}"
state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/wm"
mkdir -p "$state_dir"
# A private bus prevents the development shell from taking over another desktop's services.
exec dbus-run-session -- ./target/debug/wm --tty-udev >> "$state_dir/session.log" 2>&1
