#!/usr/bin/env python3
"""Exercise the native SCTK bar's exported D-Bus tray menu path."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from importlib.util import module_from_spec, spec_from_file_location
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
spec = spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = module_from_spec(spec)
spec.loader.exec_module(nested)


with tempfile.TemporaryDirectory(prefix="wm-sctk-menu-") as temporary:
    directory = Path(temporary)
    config = directory / "config.toml"
    config.write_text('[shell]\nbackend="sctk"\nmodules=["tray"]\n[theme]\nopacity=1.0\nanimation_ms=0\n')
    socket = directory / "wm.sock"
    title = directory.name
    log_path = directory / "session.log"
    with log_path.open("w") as log:
        process = subprocess.Popen(
            [str(ROOT / "tools/run-nested.sh")], cwd=ROOT,
            env=dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket), WM_NESTED_TITLE=title),
            stdout=log, stderr=log,
        )
        try:
            nested.wait_for(socket, lambda state: any(layer["namespace"] == "wm-bar" for layer in state["layers"]), process)
            window = subprocess.check_output(["xdotool", "search", "--name", f"^{title}$"], text=True).strip().splitlines()[-1]
            subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True)
            nested.fit_private_host(socket, process, window)
            command = ["/usr/bin/python3", str(ROOT / "tools/tray-menu-item.py"), str(directory)]
            assert nested.request(socket, "exec " + json.dumps(command))["ok"]
            deadline = time.monotonic() + 8
            while not (directory / "ready").exists():
                assert process.poll() is None and time.monotonic() < deadline
                time.sleep(0.05)
            state = nested.request(socket, "status")["state"]
            bar = next(layer for layer in state["layers"] if layer["namespace"] == "wm-bar")
            screenshot = directory / "bar.png"
            deadline = time.monotonic() + 6
            icon = None
            while icon is None:
                subprocess.run(["import", "-window", window, str(screenshot)], check=True)
                with Image.open(screenshot) as source:
                    pixels = source.convert("RGB")
                    points = [
                        (x, y)
                        for y in range(min(40, pixels.height))
                        for x in range(pixels.width)
                        if (lambda color: color[1] > 150 and color[0] < 90 and color[2] < 140)(pixels.getpixel((x, y)))
                    ]
                if points:
                    icon = (
                        (min(point[0] for point in points) + max(point[0] for point in points)) // 2,
                        (min(point[1] for point in points) + max(point[1] for point in points)) // 2,
                    )
                else:
                    assert process.poll() is None and time.monotonic() < deadline
                    time.sleep(0.05)
            subprocess.run([
                "xdotool", "mousemove", "--window", window,
                str(icon[0]), str(icon[1]), "click", "3",
            ], check=True)
            menu = nested.wait_for(
                socket,
                lambda current: next((layer for layer in current["layers"] if layer["namespace"] == "wm-tray-menu" and layer.get("surface_size", [1, 1])[0] > 1), None),
                process,
            )
            menu = next(layer for layer in menu["layers"] if layer["namespace"] == "wm-tray-menu" and layer.get("surface_size", [1, 1])[0] > 1)
            subprocess.run(["import", "-window", window, "/tmp/luma-sctk-tray-menu.png"], check=True)
            subprocess.run([
                "xdotool", "mousemove", "--window", window,
                str(menu["geometry"]["x"] + 40), str(menu["geometry"]["y"] + 14),
                "click", "1",
            ], check=True)
            deadline = time.monotonic() + 5
            while not (directory / "menu-event").exists():
                assert process.poll() is None and time.monotonic() < deadline
                time.sleep(0.05)
            assert (directory / "menu-event").read_text() == "1:clicked"
            assert not (directory / "legacy-called").exists()
            print("PASS: SCTK tray host renders and activates exported D-Bus menus")
        finally:
            Path("/tmp/luma-sctk-tray-menu.log").write_text(log_path.read_text(errors="replace"))
            if process.poll() is None:
                nested.request(socket, "quit")
                process.wait(timeout=5)
