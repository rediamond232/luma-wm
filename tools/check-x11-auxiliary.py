#!/usr/bin/env python3
"""Verify that X11 warnings and notification-style windows are not tiled."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nested)


def main():
    with tempfile.TemporaryDirectory(prefix="wm-x11-aux-") as temp:
        directory = Path(temp)
        binary = directory / "x11-auxiliary-client"
        flags = subprocess.check_output(
            ["pkg-config", "--cflags", "--libs", "xcb"], text=True
        ).split()
        subprocess.run(
            [
                "cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
                str(ROOT / "tools/x11-auxiliary-client.c"), "-o", str(binary), *flags,
            ],
            check=True,
        )

        socket = directory / "wm-nested.sock"
        config = directory / "config.toml"
        click_receipt = directory / "warning-clicked"
        config.write_text('[theme]\nanimation_ms=0\nblur=false\n[shell]\nenabled=false\n')
        title = "wm-x11-aux-" + directory.name
        env = dict(
            os.environ,
            WM_CONFIG=str(config),
            WM_SOCKET=str(socket),
            WM_NESTED_TITLE=title,
            LUMA_X11_AUX_CLICK_FILE=str(click_receipt),
        )
        log_path = directory / "session.log"
        with log_path.open("w+") as log:
            process = subprocess.Popen(
                [str(ROOT / "tools/run-nested.sh")],
                cwd=ROOT,
                env=env,
                stdout=log,
                stderr=log,
            )
            try:
                nested.wait_for(socket, lambda state: bool(state["outputs"]), process)
                # The nested output can appear just before XWayland publishes DISPLAY.
                time.sleep(1)
                result = nested.request(socket, "exec " + json.dumps([str(binary)]))
                assert result["ok"], result
                state = nested.wait_for(
                    socket,
                    lambda value: len([
                        window for window in value["windows"]
                        if window["title"].startswith("X11 ")
                    ]) == 3 and any(
                        window["title"] == "X11 notification fixture"
                        and window["geometry"] is not None
                        and window["geometry"]["x"] == 620
                        for window in value["windows"]
                    ),
                    process,
                )
                windows = {window["title"]: window for window in state["windows"]}
                normal = windows["X11 normal fixture"]
                warning = windows["X11 warning fixture"]
                notification = windows["X11 notification fixture"]
                assert not normal["floating"], normal
                assert warning["floating"], warning
                assert notification["floating"], notification
                assert (warning["geometry"]["w"], warning["geometry"]["h"]) == (300, 140), warning
                assert (notification["geometry"]["w"], notification["geometry"]["h"]) == (240, 100), notification
                assert notification["geometry"]["x"] == 620, notification
                assert notification["geometry"]["y"] == 80, notification

                host_window = subprocess.check_output(
                    ["xdotool", "search", "--name", "^" + title + "$"], text=True
                ).strip().splitlines()[-1]
                subprocess.run(
                    ["xdotool", "windowactivate", "--sync", host_window],
                    check=True,
                    timeout=5,
                )
                subprocess.run(
                    [
                        "xdotool", "mousemove", "--window", host_window,
                        str(warning["geometry"]["x"] + warning["geometry"]["w"] // 2),
                        str(warning["geometry"]["y"] + warning["geometry"]["h"] // 2),
                        "click", "1",
                    ],
                    check=True,
                    timeout=5,
                )
                deadline = time.monotonic() + 3
                while not click_receipt.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)
                assert click_receipt.read_text() == "warning clicked\n"

                screenshot = directory / "x11-auxiliary.png"
                result = nested.request(
                    socket,
                    "exec " + json.dumps(["grim", "-t", "png", str(screenshot)]),
                )
                assert result["ok"], result
                deadline = time.monotonic() + 10
                while not screenshot.exists() and time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise RuntimeError(log_path.read_text(errors="replace")[-5000:])
                    time.sleep(0.05)
                assert screenshot.exists(), log_path.read_text(errors="replace")[-5000:]
                destination = Path("/tmp/luma-x11-auxiliary-fixed.png")
                destination.write_bytes(screenshot.read_bytes())
                print(
                    "PASS: X11 warning floats, receives pointer input, and notification "
                    f"preserves placement; {destination}"
                )
            finally:
                try:
                    nested.request(socket, "quit")
                except Exception:
                    pass
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=5)
                log.flush()
                Path("/tmp/luma-x11-auxiliary.log").write_text(
                    log_path.read_text(errors="replace")
                )


if __name__ == "__main__":
    main()
