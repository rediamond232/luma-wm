#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $(id -u) == 0 ]]; then echo 'Run from your normal user login on TTY3, without sudo.' >&2; exit 1; fi
if [[ $(tty) != /dev/tty3 ]]; then echo 'Log in on TTY3 and run this command there.' >&2; exit 1; fi
./tools/build-dev.sh --offline
user_config="${XDG_CONFIG_HOME:-$HOME/.config}/wm/config.toml"
if [[ -f "$user_config" ]]; then
    export WM_CONFIG="${WM_CONFIG:-$user_config}"
else
    export WM_CONFIG="${WM_CONFIG:-$PWD/config/default.toml}"
fi
export WM_SOCKET="${XDG_RUNTIME_DIR:?Log in through a normal system session}/wm.sock"
export RUST_BACKTRACE=1
export XDG_CURRENT_DESKTOP=wm:wlr
export WM_PRIVATE_BUS=1
export RUST_LOG="${RUST_LOG:-wm_compositor=info,smithay=warn}"
state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/wm"
mkdir -p "$state_dir"
log_file="$state_dir/session.log"
: > "$log_file"
printf 'Starting wm with config %s\nLog: %s\n' "$WM_CONFIG" "$log_file"
# A private bus prevents the development shell from taking over another desktop's services.
set +e
dbus-run-session -- ./target/debug/wm --tty-udev >> "$log_file" 2>&1
status=$?
set -e
printf '\nwm exited with status %s. Last log lines:\n' "$status" >&2
tail -n 30 "$log_file" >&2
exit "$status"
