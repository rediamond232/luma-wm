#!/usr/bin/env python3
"""Build an ext-image-copy-capture client and verify real SHM frame delivery."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from importlib.util import spec_from_file_location, module_from_spec
from PIL import Image, ImageChops

ROOT = Path(__file__).resolve().parent.parent
spec = spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = module_from_spec(spec)
spec.loader.exec_module(nested)


def main():
    with tempfile.TemporaryDirectory(prefix="wm-capture-") as temp:
        directory = Path(temp)
        protocol_dir = Path(subprocess.check_output(["pkg-config", "--variable=pkgdatadir", "wayland-protocols"], text=True).strip())
        sources = []
        for protocol, name in [("ext-image-capture-source", "capture-source"), ("ext-image-copy-capture", "copy-capture"), ("ext-foreign-toplevel-list", "toplevel-list")]:
            xml = protocol_dir / "staging" / protocol / (protocol + "-v1.xml")
            subprocess.run(["wayland-scanner", "client-header", str(xml), str(directory / (name + ".h"))], check=True)
            source = directory / (name + ".c")
            subprocess.run(["wayland-scanner", "private-code", str(xml), str(source)], check=True)
            sources.append(str(source))
        client = directory / "capture-client"
        subprocess.run(["cc", "-I" + str(directory), str(ROOT / "tools/capture-client.c"), *sources,
                        "-lwayland-client", "-o", str(client)], check=True)
        wallpaper = directory / "wallpaper.png"
        pattern = Image.new("RGB", (8, 8), (32, 64, 96))
        for y in range(4, 8):
            for x in range(8): pattern.putpixel((x, y), (160, 64, 32))
        pattern.save(wallpaper)
        config = directory / "config.toml"
        config.write_text(f'[theme]\nblur=false\nanimation_ms=0\n[wallpaper]\npath={json.dumps(str(wallpaper))}\n')
        path = directory / "wm-nested.sock"
        title = directory.name
        env = dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(path), WM_NESTED_TITLE=title)
        log_path = directory / "session.log"
        with log_path.open("w") as log:
            process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT, env=env, stdout=log, stderr=log)
            def command(cmd):
                result = nested.request(path, cmd)
                assert result["ok"], result
                return result["state"]
            def capture(name, resize=False, cursor=False):
                image_path = directory / (name + ".ppm")
                command("exec " + json.dumps([str(client), str(image_path)] + (["--resize"] if resize else ["--cursor"] if cursor else [])))
                deadline = time.monotonic() + 10
                resized = False
                while time.monotonic() < deadline:
                    if resize and not resized and Path(str(image_path) + ".resize-ready").exists():
                        wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
                        subprocess.run(["xdotool", "windowsize", wid, "960", "640"], check=True)
                        resized = True
                    try:
                        if not Path(str(image_path) + ".ok").exists():
                            time.sleep(.05)
                            continue
                        with Image.open(image_path) as image:
                            image.load()
                            return image.copy()
                    except (OSError, ValueError):
                        time.sleep(.05)
                raise AssertionError(log_path.read_text(errors="replace")[-5000:])
            try:
                nested.wait_for(path, lambda s: any(l["namespace"] == "wm-wallpaper" for l in s["layers"]), process)
                if os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
                    # Tiled host windows may ignore X11 configure requests.
                    # Select only this test's uniquely titled window.
                    clients = json.loads(subprocess.check_output(["hyprctl", "-j", "clients"], text=True))
                    matches = [c for c in clients if c["title"] == title]
                    assert len(matches) == 1, matches
                    selector = "address:" + matches[0]["address"]
                    result = subprocess.run(["hyprctl", "dispatch", "setfloating", selector], capture_output=True, text=True)
                    if result.returncode and "lua" in result.stdout.lower():
                        subprocess.run(["hyprctl", "dispatch", 'hl.dsp.window.float({action="set", window=' + json.dumps(selector) + '})'], check=True, stdout=subprocess.DEVNULL)
                    else:
                        result.check_returncode()
                    clients = json.loads(subprocess.check_output(["hyprctl", "-j", "clients"], text=True))
                    assert next(c for c in clients if c["address"] == matches[0]["address"])["floating"]
                time.sleep(.5)
                image = capture("background")
                top = image.getpixel((20, image.height // 4))
                bottom = image.getpixel((20, image.height * 3 // 4))
                assert top == (32, 64, 96) and bottom == (160, 64, 32), (top, bottom, image.size)
                resized_image = capture("resized", resize=True)
                assert resized_image.size != image.size, (resized_image.size, image.size)
                assert resized_image.getpixel((20, resized_image.height // 4)) == (32, 64, 96)
                assert resized_image.getpixel((20, resized_image.height * 3 // 4)) == (160, 64, 32)
                base_config = config.read_text()
                output_name = command("status")["outputs"][0]["name"]
                for transform in ("90", "180", "270", "flipped", "flipped-90", "flipped-180", "flipped-270", "normal"):
                    config.write_text(base_config + f'\n[outputs.{json.dumps(output_name)}]\ntransform={json.dumps(transform)}\n')
                    command("reload")
                    expected = resized_image.size[::-1] if transform in ("90", "270", "flipped-90", "flipped-270") else resized_image.size
                    nested.wait_for(path, lambda s: (s["outputs"][0]["geometry"]["w"], s["outputs"][0]["geometry"]["h"]) == expected, process)
                    time.sleep(.15)
                    rotated = capture("transform-" + transform)
                    assert rotated.size == expected, (transform, rotated.size, expected)
                    assert rotated.getpixel((20, rotated.height // 4)) == (32, 64, 96), transform
                    assert rotated.getpixel((20, rotated.height * 3 // 4)) == (160, 64, 32), transform
                config.write_text(base_config)
                command("reload")
                print("PASS: upright SHM capture dimensions and pixels across all output rotations/reflections")
                wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
                subprocess.run(["xdotool", "windowactivate", "--sync", wid], check=True, timeout=5)
                subprocess.run(["xdotool", "mousemove", "--window", wid, "200", "200"], check=True, timeout=5)
                time.sleep(.2)
                without_cursor = capture("without-cursor")
                with_cursor = capture("with-cursor", cursor=True)
                changed = ImageChops.difference(without_cursor, with_cursor).getbbox()
                assert changed is not None, "cursor-inclusive capture contains no cursor"
                assert 150 <= changed[0] <= 210 and 150 <= changed[1] <= 210 and changed[2] < 300 and changed[3] < 300, changed
                cursor_image = directory / "cursor.png"
                Image.new("RGBA", (16, 20), (0, 255, 128, 255)).save(cursor_image)
                command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "WM_TEST_CURSOR=" + str(cursor_image), "python3", str(ROOT / "tools/interaction-client.py")]))
                state = nested.wait_for(path, lambda s: bool(s["windows"]) and s["windows"][0]["geometry"] is not None, process)
                time.sleep(.3)
                image = capture("window")
                rect = state["windows"][0]["geometry"]
                assert image.getpixel((rect["x"] + 30, rect["y"] + 30)) == (192, 128, 64)
                config.write_text(base_config + f'\n[outputs.{json.dumps(output_name)}]\ntransform="90"\n')
                command("reload")
                state = nested.wait_for(path, lambda s: s["outputs"][0]["geometry"]["w"] == resized_image.height
                    and s["windows"][0]["geometry"]["w"] < resized_image.height, process)
                time.sleep(.2)
                rotated_window = capture("rotated-window")
                rect = state["windows"][0]["geometry"]
                assert rotated_window.getpixel((rect["x"] + 30, rect["y"] + 30)) == (192, 128, 64)
                rotated_cursor = capture("rotated-cursor", cursor=True)
                cursor_bounds = ImageChops.difference(rotated_window, rotated_cursor).getbbox()
                assert cursor_bounds is not None and (cursor_bounds[2] - cursor_bounds[0], cursor_bounds[3] - cursor_bounds[1]) == (16, 20), cursor_bounds
                assert rotated_cursor.getpixel((cursor_bounds[0] + 2, cursor_bounds[1] + 2)) == (0, 255, 128)
                config.write_text(base_config + f'\n[outputs.{json.dumps(output_name)}]\ntransform="normal"\n')
                command("reload")
                nested.wait_for(path, lambda s: s["outputs"][0]["geometry"]["w"] == resized_image.width, process)
                config.write_text(base_config)
                time.sleep(.2)
                subprocess.run(["xdotool", "mousemove", "--window", wid, "200", "200"], check=True, timeout=5)
                time.sleep(.2)
                surface_plain = capture("surface-plain")
                surface_cursor = capture("surface-cursor", cursor=True)
                changed = ImageChops.difference(surface_plain, surface_cursor).getbbox()
                assert changed == (193, 191, 209, 211), changed
                assert surface_cursor.getpixel((200, 200)) == (0, 255, 128)
                command("close")
                nested.wait_for(path, lambda s: not s["windows"], process)
                command("exec " + json.dumps(["env", "WAYLAND_DEBUG=client", "WM_TEST_COLOR=#c08040", "WM_TEST_APP_ID=org.customwm.CaptureText", "WM_TEST_CURSOR_NAME=text", "python3", str(ROOT / "tools/interaction-client.py")]))
                nested.wait_for(path, lambda s: bool(s["windows"]), process)
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline:
                    time.sleep(.1)
                    if any("wp_cursor_shape_device_v1" in line and ".set_shape(" in line for line in log_path.read_text(errors="replace").splitlines()):
                        break
                time.sleep(.3)
                text_plain = capture("text-plain")
                text_cursor = capture("text-cursor", cursor=True)
                text_diff = ImageChops.difference(text_plain, text_cursor)
                text_bounds = text_diff.getbbox()
                assert text_bounds is not None, "text cursor missing"
                assert text_bounds[2] - text_bounds[0] < 40 and text_bounds[3] - text_bounds[1] < 60, text_bounds
                arrow_diff = ImageChops.difference(without_cursor, with_cursor)
                assert text_diff.crop(text_bounds).tobytes() != arrow_diff.crop(arrow_diff.getbbox()).tobytes(), "text shape fell back to arrow"
                assert any("wp_cursor_shape_device_v1" in line and ".set_shape(" in line for line in log_path.read_text(errors="replace").splitlines()), "client did not exercise cursor-shape protocol"
                command("close")
                nested.wait_for(path, lambda s: not s["windows"], process)
                command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "WM_TEST_APP_ID=org.customwm.CaptureHidden", "WM_TEST_CURSOR_NAME=none", "python3", str(ROOT / "tools/interaction-client.py")]))
                nested.wait_for(path, lambda s: bool(s["windows"]), process)
                subprocess.run(["xdotool", "mousemove", "--window", wid, "200", "200"], check=True, timeout=5)
                time.sleep(.3)
                hidden_plain = capture("hidden-plain")
                hidden_cursor = capture("hidden-cursor", cursor=True)
                assert ImageChops.difference(hidden_plain, hidden_cursor).getbbox() is None, "hidden cursor leaked into capture"
                command("close")
                state = nested.wait_for(path, lambda s: not s["windows"], process)
                with config.open("a") as file:
                    file.write("\n[outputs." + json.dumps(state["outputs"][0]["name"]) + "]\nscale=1.5\n")
                command("reload")
                command("exec " + json.dumps(["env", "WM_TEST_APP_ID=org.customwm.CaptureScaled", "WM_TEST_COLOR=#c08040", "WM_TEST_CURSOR=" + str(cursor_image), "python3", str(ROOT / "tools/interaction-client.py")]))
                nested.wait_for(path, lambda s: bool(s["windows"]), process)
                time.sleep(.3)
                subprocess.run(["xdotool", "mousemove", "--window", wid, "201", "201"], check=True, timeout=5)
                time.sleep(.3)
                scaled_plain = capture("scaled-plain")
                scaled_cursor = capture("scaled-cursor", cursor=True)
                scaled_bounds = ImageChops.difference(scaled_plain, scaled_cursor).getbbox()
                assert scaled_bounds is not None
                assert scaled_cursor.getpixel((201, 201)) == (0, 255, 128)
                assert 23 <= scaled_bounds[2] - scaled_bounds[0] <= 25 and 29 <= scaled_bounds[3] - scaled_bounds[1] <= 31, scaled_bounds
                assert abs(scaled_bounds[0] - (201 - 7 * 1.5)) <= 1 and abs(scaled_bounds[1] - (201 - 9 * 1.5)) <= 1, scaled_bounds
                output_name = command("status")["outputs"][0]["name"]
                for transform in ("90", "180", "270", "flipped", "flipped-90", "flipped-180", "flipped-270"):
                    config.write_text(base_config + f'\n[outputs.{json.dumps(output_name)}]\nscale=1.5\ntransform={json.dumps(transform)}\n')
                    command("reload")
                    expected = resized_image.size[::-1] if transform in ("90", "270", "flipped-90", "flipped-270") else resized_image.size
                    state = nested.wait_for(path, lambda s: abs(s["outputs"][0]["geometry"]["w"] * 1.5 - expected[0]) <= 1, process)
                    subprocess.run(["xdotool", "mousemove", "--window", wid, "200", "200", "mousemove", "--window", wid, "201", "201"], check=True, timeout=5)
                    time.sleep(.2)
                    plain = capture("scaled-transform-" + transform)
                    cursor_frame = capture("scaled-transform-cursor-" + transform, cursor=True)
                    assert plain.size == expected, (transform, plain.size, expected)
                    rect = state["windows"][0]["geometry"]
                    assert plain.getpixel((round((rect["x"] + 30) * 1.5), round((rect["y"] + 30) * 1.5))) == (192, 128, 64), transform
                    bounds = ImageChops.difference(plain, cursor_frame).getbbox()
                    native_w, native_h = resized_image.size
                    cursor_x, cursor_y = {
                        "90": (native_h - 201, 201), "180": (native_w - 201, native_h - 201),
                        "270": (201, native_w - 201), "flipped": (native_w - 201, 201),
                        "flipped-90": (201, 201), "flipped-180": (201, native_h - 201),
                        "flipped-270": (native_h - 201, native_w - 201),
                    }[transform]
                    assert bounds is not None and 23 <= bounds[2] - bounds[0] <= 25 and 29 <= bounds[3] - bounds[1] <= 31, (transform, bounds)
                    assert abs(bounds[0] - (cursor_x - 7 * 1.5)) <= 2 and abs(bounds[1] - (cursor_y - 9 * 1.5)) <= 2, (transform, bounds, cursor_x, cursor_y)
                    assert cursor_frame.getpixel((cursor_x, cursor_y)) == (0, 255, 128), transform
                print("PASS: fractional-scale capture dimensions, window pixels and cursor bounds across rotations/reflections")
                motion_config = '[theme]\nblur=false\nanimation_ms=2000\n[outputs.' + json.dumps(output_name) + ']\nscale=1.0\n'
                config.write_text(motion_config)
                command("reload")
                time.sleep(2.2)
                old_rect = command("status")["windows"][0]["geometry"]
                target = command("floating")["windows"][0]["geometry"]
                assert target["x"] != old_rect["x"], (old_rect, target)
                positions = []
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline:
                    frame = capture("movement-" + str(len(positions)))
                    delta = ImageChops.difference(frame, Image.new("RGB", frame.size, (192, 128, 64)))
                    mask = delta.convert("L").point(lambda value: 255 if value == 0 else 0)
                    bounds = mask.getbbox()
                    assert bounds, "moving window disappeared"
                    positions.append(bounds[0])
                    if abs(bounds[0] - target["x"]) <= 1:
                        break
                assert any(min(old_rect["x"], target["x"]) + 2 < x < max(old_rect["x"], target["x"]) - 2 for x in positions), positions
                assert abs(positions[-1] - target["x"]) <= 1, (positions, target)
                config.write_text(motion_config.replace("animation_ms=2000", "animation_ms=2000\nreduced_motion=true"))
                command("reload")
                target = command("floating")["windows"][0]["geometry"]
                frame = capture("movement-reduced")
                delta = ImageChops.difference(frame, Image.new("RGB", frame.size, (192, 128, 64)))
                bounds = delta.convert("L").point(lambda value: 255 if value == 0 else 0).getbbox()
                assert bounds and abs(bounds[0] - target["x"]) <= 1, (bounds, target)
                config.write_text(motion_config)
                command("reload")
                command("workspace 2")
                empty = capture("workspace-empty")
                sample_point = (target["x"] + 30, target["y"] + 30)
                assert empty.getpixel(sample_point) != (192, 128, 64), "outgoing workspace still rendered"
                command("workspace 1")
                colors = []
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline:
                    colors.append(capture("workspace-enter-" + str(len(colors))).getpixel(sample_point))
                    if colors[-1] == (192, 128, 64):
                        break
                assert colors[-1] == (192, 128, 64), colors
                assert any(color != empty.getpixel(sample_point) and color != (192, 128, 64) for color in colors), colors
                config.write_text(motion_config.replace("animation_ms=2000", "animation_ms=2000\nreduced_motion=true"))
                command("reload")
                command("workspace 2")
                command("workspace 1")
                assert capture("workspace-reduced").getpixel(sample_point) == (192, 128, 64)
                print("PASS: capture resize recovery, cursor shapes/hiding/scaling and stationary focus, movement/workspace fades and reduced motion")
            finally:
                if process.poll() is None:
                    command("quit")
                    process.wait(timeout=5)


if __name__ == "__main__":
    main()
