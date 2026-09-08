#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export WM_CONFIG="${WM_CONFIG:-$PWD/config/smoke.toml}"
export WM_SOCKET="${WM_SOCKET:-${XDG_RUNTIME_DIR:?}/wm-nested.sock}"
export XDG_CURRENT_DESKTOP=wm:wlr
export WM_PRIVATE_BUS=1
export RUST_BACKTRACE=1
export RUST_LOG="${RUST_LOG:-wm_compositor=info,smithay=warn}"
case "${WM_NESTED_BACKEND:-x11}" in
  x11) backend_flag=--x11 ;;
  winit) backend_flag=--winit ;;
  *) echo "WM_NESTED_BACKEND must be x11 or winit" >&2; exit 2 ;;
esac
if [[ -n "${WM_CAPTURE_PRIVATE_X11:-}" ]]; then
  # dbus-run-session does not provide the user-systemd instance used by the
  # packaged AT-SPI activation file. Start its bus directly so GTK applications
  # do not exit while registering on the isolated integration-test bus.
  exec dbus-run-session -- sh -c '/usr/lib/at-spi-bus-launcher --launch-immediately >/dev/null 2>&1 & exec "$@"' sh ./target/debug/wm "$backend_flag"
fi
exec dbus-run-session -- ./target/debug/wm "$backend_flag"
