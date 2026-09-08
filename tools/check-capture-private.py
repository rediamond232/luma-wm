#!/usr/bin/env python3
"""Run an integration script on a separate rootful Xwayland server and clean it up."""
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def main():
    with tempfile.TemporaryDirectory(prefix="wm-private-x11-") as directory:
        log_path = Path(directory) / "xwayland.log"
        with log_path.open("w") as log:
            server = subprocess.Popen(
                ["Xwayland", "-displayfd", "1", "-geometry", "1280x900", "-nolisten", "tcp", "-noreset"],
                stdout=subprocess.PIPE, stderr=log, text=True,
            )
            test = None
            try:
                if not select.select([server.stdout], [], [], 10)[0]:
                    raise RuntimeError("private Xwayland did not report a display within ten seconds")
                display = server.stdout.readline().strip()
                if not display.isdecimal() or server.poll() is not None:
                    raise RuntimeError("private Xwayland failed: " + log_path.read_text())
                env = os.environ.copy()
                env["DISPLAY"] = ":" + display
                env["WM_CAPTURE_PRIVATE_X11"] = "1"
                # The isolated session intentionally has no desktop accessibility
                # bus. Keep GTK clients from repeatedly trying to activate one.
                env["NO_AT_BRIDGE"] = "1"
                env["GTK_A11Y"] = "none"
                env.pop("HYPRLAND_INSTANCE_SIGNATURE", None)
                if env.get("WM_NESTED_BACKEND") == "winit":
                    env.pop("WAYLAND_DISPLAY", None)
                    env.pop("WAYLAND_SOCKET", None)
                script = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "tools/check-capture.py"
                test = subprocess.Popen(
                    [sys.executable, str(script), *sys.argv[2:]],
                    cwd=ROOT,
                    env=env,
                    start_new_session=True,
                )
                return test.wait(timeout=600)
            finally:
                if server.poll() is not None:
                    Path("/tmp/luma-private-xwayland-failure.log").write_text(log_path.read_text(errors="replace"))
                if test is not None and test.poll() is None:
                    os.killpg(test.pid, signal.SIGTERM)
                    try:
                        test.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(test.pid, signal.SIGKILL)
                        test.wait()
                if server.poll() is None:
                    server.terminate()
                    try:
                        server.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()
                server.stdout.close()


if __name__ == "__main__":
    sys.exit(main())
