#!/usr/bin/env python3
"""Validate a real VLC tray icon/menu in an isolated nested desktop."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from importlib.util import spec_from_file_location, module_from_spec
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
spec = spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = module_from_spec(spec)
spec.loader.exec_module(nested)

with tempfile.TemporaryDirectory(prefix="wm-tray-vlc-") as temporary:
    directory = Path(temporary)
    config = directory / "config.toml"
    config.write_text('[shell]\nmodules=["tray"]\n[theme]\nopacity=1.0\nanimation_ms=0\n')
    socket = directory / "wm.sock"
    title = directory.name
    log_path = directory / "session.log"
    with log_path.open("w") as log:
        process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT,
            env=dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket), WM_NESTED_TITLE=title), stdout=log, stderr=log)
        try:
            nested.wait_for(socket, lambda s: any(l["namespace"] == "wm-bar" for l in s["layers"]), process)
            wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
            subprocess.run(["xdotool", "windowactivate", "--sync", wid], check=True, timeout=5)
            assert nested.request(socket, "exec " + json.dumps(["/usr/bin/python3", str(ROOT / "tools/tray-vlc-client.py"), str(directory)]))["ok"]
            def wait(condition):
                deadline = time.monotonic() + 18
                while not condition():
                    assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(errors="replace")[-5000:]
                    time.sleep(.05)
            wait(lambda: (directory / "vlc-ready.json").exists())
            data = json.loads((directory / "vlc-ready.json").read_text())
            rows = [row for row in data["layout"][1][2] if row[1].get("visible",True) and row[1].get("enabled",True)]
            assert "quit" in rows[-1][1]["label"].replace("_","").lower(), rows
            def icon_bounds():
                path = directory / "screen.png"
                subprocess.run(["import", "-window", wid, str(path)], check=True)
                with Image.open(path) as source:
                    image = source.convert("RGB").crop((0,0,source.width,36))
                    mask = Image.new("L",image.size)
                    pixels = image.tobytes()
                    mask.putdata([255 if r > 150 and 40 < g < 210 and b < 100 else 0 for r,g,b in zip(pixels[0::3],pixels[1::3],pixels[2::3])])
                    return mask.getbbox()
            wait(lambda: icon_bounds() is not None)
            box = icon_bounds()
            subprocess.run(["xdotool", "mousemove", "--window", wid, str((box[0]+box[2])//2), str((box[1]+box[3])//2), "click", "3"],check=True)
            def menu_ready():
                path = directory / "menu.png"
                subprocess.run(["import", "-window", wid, str(path)],check=True)
                with Image.open(path) as source:
                    pixels = source.convert("RGB").crop((max(0,source.width-400),40,source.width,min(source.height,480))).tobytes()
                    return any(r > 150 and 40 < g < 210 and b < 100 for r,g,b in zip(pixels[0::3],pixels[1::3],pixels[2::3]))
            wait(menu_ready)
            subprocess.run(["import", "-window", wid, "/tmp/luma-tray-vlc.png"],check=True)
            # The menu focuses its first action; Shift+Tab wraps to the last.
            subprocess.run(["xdotool", "key", "shift+Tab", "Return"],check=True)
            wait(lambda: (directory / "vlc-exited").exists())
            wait(lambda: icon_bounds() is None)
            print("PASS: real VLC tray registration/icon, exported menu, keyboard Quit action and removal")
        finally:
            (directory / "stop").write_text("ok")
            for source, target in ((log_path,"/tmp/luma-tray-vlc.log"), (directory / "vlc.log","/tmp/luma-tray-vlc-app.log"),
                                   (directory / "vlc-ready.json","/tmp/luma-tray-vlc-menu.json")):
                if source.exists():
                    Path(target).write_text(source.read_text(errors="replace"))
            if process.poll() is None:
                nested.request(socket, "quit")
                process.wait(timeout=5)
