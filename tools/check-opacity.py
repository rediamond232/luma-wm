#!/usr/bin/env python3
"""Verify compositor opacity using captured pixels, without moving the pointer."""
import json
import os
import signal
from pathlib import Path
import subprocess
import tempfile
import time
from importlib.util import spec_from_file_location, module_from_spec
from PIL import Image, ImageStat

ROOT = Path(__file__).resolve().parent.parent
spec = spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = module_from_spec(spec)
spec.loader.exec_module(nested)


def main():
    with tempfile.TemporaryDirectory(prefix="wm-opacity-") as directory:
        directory = Path(directory)
        path = directory / "wm-nested.sock"
        config = directory / "config.toml"
        title = "wm-test-" + directory.name
        base = '''[theme]
background = "#204060"
blur = false
border = 6
shadow_size = 0
accent = "#ff0000"
muted = "#00ff00"
animation_ms = 2000
reduced_motion = false
[[rules]]
app_id = "org.customwm.InteractionTest"
floating = true
width = 320
height = 200
'''
        config.write_text(base + "opacity = 1.0\n")
        env = dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(path), WM_NESTED_TITLE=title)
        log_path = directory / "session.log"
        with log_path.open("w") as log:
            process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT, env=env, stdout=log, stderr=log)
            def command(text):
                result = nested.request(path, text)
                assert result["ok"], result
                return result["state"]
            def window(state):
                return next((w for w in state["windows"] if w["app_id"] == "org.customwm.InteractionTest"), None)
            try:
                nested.wait_for(path, lambda s: any(l["namespace"] == "wm-bar" for l in s["layers"]), process)
                pid_file = directory / "client.pid"
                command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", f"WM_TEST_PID_FILE={pid_file}", "python3", str(ROOT / "tools/interaction-client.py")]))
                nested.wait_for(path, lambda s: window(s) is not None and window(s)["title"] == "Interaction test 320x200", process)
                wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
                samples = []
                def pixel(expected, border=False, outside=None):
                    deadline = time.monotonic() + 5
                    actual = None
                    while time.monotonic() < deadline:
                        rect = window(command("status"))["geometry"]
                        screenshot = directory / "frame.png"
                        subprocess.run(["import", "-window", wid, str(screenshot)], check=True)
                        with Image.open(screenshot) as image:
                            point = (rect["x"] - 3, rect["y"] + rect["h"] // 2) if border else (rect["x"] + 30, rect["y"] + 30)
                            if outside is not None:
                                point = (rect["x"] - outside, rect["y"] + rect["h"] // 2)
                            actual = image.convert("RGB").getpixel(point)
                            samples.append(actual)
                        if all(abs(a - b) <= 3 for a, b in zip(actual, expected)):
                            return
                        time.sleep(.1)
                    raise AssertionError(f"expected pixel {expected}, got {actual}")
                pixel((192, 128, 64))
                assert any(40 < sample[0] < 180 for sample in samples), f"no intermediate opening frames: {samples}"
                print("PASS: opening fade renders intermediate and final client pixels")
                pixel((255, 0, 0), border=True)
                if os.environ.get("WM_CHECK_BORDER_SCREENSHOT"):
                    import shutil
                    shutil.copyfile(directory / "frame.png", os.environ["WM_CHECK_BORDER_SCREENSHOT"])
                command("exec " + json.dumps(["env", "WM_TEST_APP_ID=org.customwm.OtherTest", "python3", str(ROOT / "tools/interaction-client.py")]))
                other_state = nested.wait_for(path, lambda s: any(w["app_id"] == "org.customwm.OtherTest" for w in s["windows"]), process)
                command("focus " + str(next(w["id"] for w in other_state["windows"] if w["app_id"] == "org.customwm.OtherTest")))
                pixel((0, 255, 0), border=True)
                command("close")
                nested.wait_for(path, lambda s: not any(w["app_id"] == "org.customwm.OtherTest" for w in s["windows"]), process)
                pixel((255, 0, 0), border=True)
                config.write_text(base.replace("border = 6", "border = 0") + "opacity = 1.0\n")
                command("reload")
                pixel((32, 64, 96), border=True)
                config.write_text(base + "opacity = 1.0\n")
                command("reload")
                pixel((255, 0, 0), border=True)
                print("PASS: exterior border, focus colors and live disable/restore")
                pixel((32, 64, 96), outside=12)
                shadow_config = base.replace("shadow_size = 0", "shadow_size = 16\nshadow_opacity = 0.5") + "opacity = 1.0\n"
                config.write_text(shadow_config)
                command("reload")
                pixel((20, 41, 61), outside=12)
                pixel((192, 128, 64))
                config.write_text(shadow_config.replace("shadow_opacity = 0.5", "shadow_opacity = 0.0"))
                command("reload")
                pixel((32, 64, 96), outside=12)
                config.write_text(base + "opacity = 1.0\n")
                command("reload")
                print("PASS: exterior shadow falloff, unchanged client pixels and live disable")
                for opacity, expected in [(0.5, (112, 96, 80)), (0.0, (32, 64, 96)), (0.5, (112, 96, 80))]:
                    config.write_text(base + f"opacity = {opacity}\n")
                    command("reload")
                    nested.wait_for(path, lambda s: window(s)["opacity"] == opacity, process)
                    pixel(expected)
                command("focus " + str(window(command("status"))["id"]))
                command("fullscreen")
                pixel((192, 128, 64))
                command("fullscreen")
                pixel((112, 96, 80))
                # A later matching rule overrides opacity, while a later rule
                # without opacity must not reset the previously selected value.
                config.write_text(base + '''opacity = 0.5
[[rules]]
app_id = "org.customwm.InteractionTest"
opacity = 0.25
[[rules]]
app_id = "org.customwm.InteractionTest"
floating = true
''')
                command("reload")
                nested.wait_for(path, lambda s: window(s)["opacity"] == 0.25, process)
                pixel((72, 80, 88))
                config.write_text(base)
                command("reload")
                nested.wait_for(path, lambda s: window(s)["opacity"] == 1.0, process)
                pixel((192, 128, 64))
                print("PASS: opacity pixels, fullscreen restoration, rule precedence and removal")
                # A high-frequency backdrop distinguishes actual blur from an
                # opacity-only change. Sample away from text and rounded edges.
                with Image.open(directory / "frame.png") as frame:
                    size = frame.size
                pattern = Image.new("RGB", size)
                pattern.putdata([
                    (240, 240, 240) if (x // 8 + y // 8) % 2 else (16, 16, 16)
                    for y in range(size[1]) for x in range(size[0])
                ])
                wallpaper = directory / "checker.png"
                pattern.save(wallpaper)
                blur_base = base.replace("blur = false", "blur = true")
                def set_blur(enabled, global_enabled=True, passes=3):
                    theme_base = blur_base.replace("blur = true", f"blur = {str(global_enabled).lower()}\nblur_passes = {passes}", 1)
                    config.write_text(theme_base + f'''opacity = 0.5
blur = {str(enabled).lower()}
[wallpaper]
path = {json.dumps(str(wallpaper))}
''')
                    command("reload")
                def contrast():
                    rect = window(command("status"))["geometry"]
                    screenshot = directory / "contrast.png"
                    subprocess.run(["import", "-window", wid, str(screenshot)], check=True)
                    with Image.open(screenshot) as frame:
                        region = frame.convert("RGB").crop((rect["x"] + 30, rect["y"] + 30,
                                                           rect["x"] + 110, rect["y"] + 65))
                        return ImageStat.Stat(region).stddev[0]
                def wait_contrast(predicate):
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        value = contrast()
                        if predicate(value):
                            return value
                        time.sleep(.1)
                    raise AssertionError(f"unexpected backdrop contrast: {value}")
                set_blur(False)
                sharp = wait_contrast(lambda value: value > 40)
                set_blur(True)
                soft = wait_contrast(lambda value: value < sharp * .7)
                set_blur(False)
                wait_contrast(lambda value: value > sharp * .9)
                set_blur(True)
                wait_contrast(lambda value: value < sharp * .7)
                set_blur(True, global_enabled=False)
                wait_contrast(lambda value: value > sharp * .9)
                set_blur(True, passes=0)
                wait_contrast(lambda value: value > sharp * .9)
                set_blur(True)
                wait_contrast(lambda value: value < sharp * .7)
                print(f"PASS: backdrop blur ({sharp:.1f} -> {soft:.1f}), rule/global disable, zero strength and restore")
                rect = window(command("status"))["geometry"]
                point = (rect["x"] + 30, rect["y"] + 30)
                expected = pattern.getpixel(point)
                os.kill(int(pid_file.read_text()), signal.SIGKILL)
                nested.wait_for(path, lambda s: window(s) is None, process)
                deadline = time.monotonic() + 5
                while True:
                    screenshot = directory / "disconnected.png"
                    subprocess.run(["import", "-window", wid, str(screenshot)], check=True)
                    with Image.open(screenshot) as frame:
                        actual = frame.convert("RGB").getpixel(point)
                    if all(abs(a-b) <= 3 for a,b in zip(actual, expected)):
                        break
                    assert time.monotonic() < deadline, f"stale disconnected client pixels: {actual} != {expected}"
                    time.sleep(.1)
                print("PASS: abrupt client disconnect repaints the exposed wallpaper")
                config.write_text(base.replace("reduced_motion = false", "reduced_motion = true") + "opacity = 1.0\n")
                command("reload")
                command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "python3", str(ROOT / "tools/interaction-client.py")]))
                nested.wait_for(path, lambda s: window(s) is not None and window(s)["title"] == "Interaction test 320x200", process)
                samples.clear()
                pixel((192, 128, 64))
                assert not any(40 < sample[0] < 180 for sample in samples), f"reduced-motion opening still faded: {samples}"
                print("PASS: reduced motion skips opening fade")
            except BaseException:
                print(log_path.read_text(errors="replace")[-4000:])
                raise
            finally:
                if process.poll() is None:
                    try:
                        command("quit")
                        process.wait(timeout=5)
                    except (OSError, subprocess.TimeoutExpired):
                        process.terminate()
                        process.wait(timeout=5)


if __name__ == "__main__":
    main()
