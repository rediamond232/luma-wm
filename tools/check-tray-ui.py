#!/usr/bin/env python3
"""Verify tray pixmap rendering, pointer actions and item lifecycle in the bar."""
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

with tempfile.TemporaryDirectory(prefix="wm-tray-ui-") as temporary:
    directory = Path(temporary)
    for name, color in (("icons-a", (240,140,30)), ("icons-b", (130,60,210))):
        (directory / name).mkdir()
        Image.new("RGB",(18,18),color).save(directory / name / "luma-fixture-icon.png")
    config = directory / "config.toml"
    config.write_text('[shell]\nmodules=["tray"]\n[theme]\nopacity=1.0\nanimation_ms=0\n')
    (directory / "item-state").write_text("active")
    socket = directory / "wm.sock"
    title = directory.name
    log_path = directory / "session.log"
    with log_path.open("w") as log:
        process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT,
                                   env=dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket), WM_NESTED_TITLE=title), stdout=log, stderr=log)
        try:
            nested.wait_for(socket, lambda s: any(l["namespace"] == "wm-bar" for l in s["layers"]), process)
            wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
            if not os.environ.get("WM_CAPTURE_PRIVATE_X11"):
                subprocess.run(["xdotool", "windowactivate", "--sync", wid], check=True)
            nested.fit_private_host(socket, process, wid)
            assert nested.request(socket, "exec " + json.dumps(["/usr/bin/python3", str(ROOT / "tools/tray-item.py"), str(directory)]))["ok"]
            def bounds(color=(32, 180, 80)):
                image_path = directory / "tray.png"
                subprocess.run(["import", "-window", wid, str(image_path)], check=True)
                with Image.open(image_path) as source:
                    image = source.convert("RGB")
                    difference = ImageChops.difference(image, Image.new("RGB", image.size, color))
                    return difference.convert("L").point(lambda p: 255 if p == 0 else 0).getbbox()
            def wait(condition):
                deadline = time.monotonic() + 6
                while True:
                    result = condition()
                    if result:
                        return result
                    if process.poll() is not None or time.monotonic() >= deadline:
                        subprocess.run(
                            ["import", "-window", wid, "/tmp/luma-tray-ui-failure.png"],
                            check=False,
                        )
                        diagnostics = {
                            path.name: path.read_text(errors="replace")
                            for path in directory.iterdir()
                            if path.is_file()
                            and path.suffix != ".png"
                            and path.stat().st_size < 4096
                        }
                        diagnostics["compositor-status"] = nested.request(socket, "status")
                        diagnostics["compositor-returncode"] = process.poll()
                        diagnostics["x11-display"] = subprocess.check_output(
                            ["xdotool", "getdisplaygeometry"], text=True
                        ).strip()
                        diagnostics["x11-window"] = subprocess.check_output(
                            ["xdotool", "getwindowgeometry", "--shell", wid], text=True
                        ).strip()
                        raise AssertionError(
                            log_path.read_text(errors="replace")[-4000:] + "\n" + repr(diagnostics)
                        )
                    time.sleep(.05)
            wait(lambda: (directory / "item-ready").exists())
            wait(lambda: bounds() is not None)
            original = config.read_text()
            for height in (64, 36):
                config.write_text(original.replace('modules=["tray"]', f'modules=["tray"]\nheight={height}'))
                assert nested.request(socket, "reload")["ok"]
                nested.wait_for(socket, lambda s: any(l["namespace"] == "wm-bar" and
                    (l["geometry"]["h"] >= 64) == (height == 64) for l in s["layers"]), process)
                wait(lambda: bounds() is not None)
            box = bounds()
            assert 16 <= box[2] - box[0] <= 20 and 16 <= box[3] - box[1] <= 20, box
            expected_action_point = ((box[0] + box[2]) // 2, (box[1] + box[3]) // 2)
            def actions():
                path = directory / "actions"
                return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
            for button, method in ((1, "Activate"), (2, "SecondaryActivate"), (3, "ContextMenu")):
                subprocess.run(["xdotool", "mousemove", "--window", wid, str((box[0] + box[2]) // 2), str((box[1] + box[3]) // 2), "click", str(button)], check=True)
                wait(lambda: (directory / "calls").exists() and method in (directory / "calls").read_text().splitlines())
                wait(lambda: len(actions()) >= button)
                action = actions()[button - 1]
                assert action[0] == method, action
                assert abs(action[1] - expected_action_point[0]) <= 3, (action, expected_action_point)
                assert abs(action[2] - expected_action_point[1]) <= 3, (action, expected_action_point)
            expected_scrolls = []
            def scrolls():
                path = directory / "scrolls"
                return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
            if not os.environ.get("WM_CHECK_TRAY_COORDS_ONLY"):
                for button, delta, axis in ((4, -1, "vertical"), (5, 1, "vertical"),
                                            (6, -1, "horizontal"), (7, 1, "horizontal")):
                    subprocess.run([
                        "xdotool", "mousemove", "--window", wid,
                        str((box[0] + box[2]) // 2), str((box[1] + box[3]) // 2),
                        "click", str(button),
                    ], check=True)
                    expected_scrolls.append([delta, axis])
                    wait(lambda: len(scrolls()) >= len(expected_scrolls))
                    assert scrolls() == expected_scrolls, scrolls()
            bottom_config = original.replace(
                'modules=["tray"]', 'modules=["tray"]\nheight=36\nposition="bottom"'
            )
            config.write_text(bottom_config)
            assert nested.request(socket, "reload")["ok"]
            bottom_state = nested.wait_for(
                socket,
                lambda state: any(
                    layer["namespace"] == "wm-bar" and layer["geometry"]["y"] > 0
                    for layer in state["layers"]
                ),
                process,
            )
            output_geometry = bottom_state["outputs"][0]["geometry"]
            def bottom_bounds():
                box = bounds()
                if box is None:
                    return None
                center_y = (box[1] + box[3]) // 2
                return box if center_y >= output_geometry["h"] - 100 else None
            wait(lambda: bottom_bounds() is not None)
            bottom_box = bottom_bounds()
            subprocess.run([
                "xdotool", "mousemove", "--window", wid,
                str((bottom_box[0] + bottom_box[2]) // 2),
                str((bottom_box[1] + bottom_box[3]) // 2),
                "click", "3",
            ], check=True)
            wait(lambda: len(actions()) >= 4)
            bottom_action = actions()[3]
            bottom_expected = (
                output_geometry["x"] + (bottom_box[0] + bottom_box[2]) // 2,
                output_geometry["y"] + (bottom_box[1] + bottom_box[3]) // 2,
            )
            assert bottom_action[0] == "ContextMenu", bottom_action
            assert abs(bottom_action[1] - bottom_expected[0]) <= 3, (bottom_action, bottom_expected)
            assert abs(bottom_action[2] - bottom_expected[1]) <= 3, (bottom_action, bottom_expected)
            if os.environ.get("WM_CHECK_TRAY_COORDS_ONLY"):
                print("PASS: tray actions receive exact top- and bottom-bar output coordinates")
                raise SystemExit(0)
            config.write_text(original)
            assert nested.request(socket, "reload")["ok"]
            nested.wait_for(
                socket,
                lambda state: any(
                    layer["namespace"] == "wm-bar" and layer["geometry"]["y"] == 0
                    for layer in state["layers"]
                ),
                process,
            )
            wait(lambda: bounds() is not None)
            box = bounds()
            subprocess.run(["xdotool", "mousemove", "--window", wid, "100", "100"],check=True)
            (directory / "item-state").write_text("overlay")
            wait(lambda: bounds((210, 40, 90)) is not None)
            badge = bounds((210, 40, 90))
            if os.environ.get("WM_CHECK_TRAY_SCREENSHOT"):
                subprocess.run(["import", "-window", wid, os.environ["WM_CHECK_TRAY_SCREENSHOT"]],check=True)
            assert 8 <= badge[2]-badge[0] <= 10 and 8 <= badge[3]-badge[1] <= 10, badge
            assert abs(badge[2]-box[2]) <= 1 and abs(badge[3]-box[3]) <= 1, (badge,box)
            (directory / "item-state").write_text("attention-overlay")
            wait(lambda: bounds((40, 110, 220)) is not None)
            assert bounds() is None, "attention icon must replace the normal icon"
            assert bounds((210, 40, 90)) == badge, "overlay must stay attached to the attention icon"
            (directory / "item-state").write_text("active")
            wait(lambda: bounds((210, 40, 90)) is None)
            assert bounds() == box, "removing an overlay must restore the base icon"
            mutated_custom_icon = False
            for command, color in (("custom-icons-a", (240,140,30)), ("custom-icons-b", (130,60,210)),
                                   ("custom-missing", (32,180,80)), ("custom-icons-a", (240,140,30)),
                                   ("custom-relative", (32,180,80)), ("custom-icons-b", (130,60,210)), ("active", (32,180,80))):
                (directory / "item-state").write_text(command)
                wait(lambda: bounds(color) is not None)
                if command == "custom-icons-a" and not mutated_custom_icon:
                    mutated_custom_icon = True
                    Image.new("RGB", (18, 18), (25, 190, 215)).save(
                        directory / "icons-a/luma-fixture-icon.png"
                    )
                    wait(lambda: bounds((25, 190, 215)) is not None)
                    Image.new("RGB", (18, 18), (240, 140, 30)).save(
                        directory / "icons-a/luma-fixture-icon.png"
                    )
                    wait(lambda: bounds((240, 140, 30)) is not None)
                if color != (32,180,80):
                    assert bounds() is None, "custom theme icon must take precedence over fallback pixmap"
            subprocess.run(["xdotool", "mousemove", "--window", wid, str((box[0]+box[2])//2), str((box[1]+box[3])//2)],check=True)
            tooltip_frames = []
            def tooltip_visible():
                box = bounds((220,110,40))
                if box is not None:
                    tooltip_frames.append(box)
                return box is not None
            wait(tooltip_visible)
            tooltip_box = tooltip_frames[-1]
            for _ in range(3):
                time.sleep(.1)
                subprocess.run([
                    "xdotool", "mousemove", "--window", wid,
                    str((box[0] + box[2]) // 2), str((box[1] + box[3]) // 2),
                ], check=True)
            assert bounds((220,110,40)) is not None, "tooltip disappeared while pointer remained over item"
            assert tooltip_box[1] >= 36 and tooltip_box[2]-tooltip_box[0] == 32 and tooltip_box[3]-tooltip_box[1] == 32, tooltip_box
            if os.environ.get("WM_CHECK_TRAY_TOOLTIP"):
                Path(os.environ["WM_CHECK_TRAY_TOOLTIP"]).write_bytes((directory / "tray.png").read_bytes())
            subprocess.run(["xdotool", "mousemove", "--window", wid, "100", "100"],check=True)
            wait(lambda: bounds((220,110,40)) is None)
            (directory / "item-state").write_text("passive")
            wait(lambda: bounds() is None)
            (directory / "item-state").write_text("active")
            wait(lambda: bounds() is not None)
            (directory / "item-state").write_text("resolution")
            wait(lambda: bounds((32, 180, 80)) is not None)
            state = nested.request(socket,"status")["state"]
            output = state["outputs"][0]
            native_width = output["geometry"]["w"]
            for scale in (2.0, 1.0, 1.5, 1.0):
                config.write_text(original + f'\n[outputs.{json.dumps(output["name"])}]\nscale={scale}\n')
                assert nested.request(socket,"reload")["ok"]
                nested.wait_for(socket, lambda s: abs(s["outputs"][0]["geometry"]["w"]*scale-native_width)<=1,process)
                color = (130,60,210) if scale > 1 else (32,180,80)
                settled = []
                def settled_icon():
                    icon = bounds(color)
                    expected = 18 * scale
                    if icon is None or abs(icon[2]-icon[0]-expected)>1 or abs(icon[3]-icon[1]-expected)>1:
                        settled.clear()
                        return None
                    settled.append(icon)
                    return icon if len(settled) >= 4 else None
                icon = wait(settled_icon)
                assert abs(icon[2]-icon[0]-18*scale)<=1 and abs(icon[3]-icon[1]-18*scale)<=1, (scale,icon)
            (directory / "item-state").write_text("quit")
            wait(lambda: bounds() is None)
            print("PASS: tray tooltip image size/hover stability/dismissal, ARGB pixmap colors/size, overlay size/placement/removal, attention icon transition, custom icon paths/in-place changes/fallback, live 1x/2x/1.5x source selection and dimensions, three pointer actions with output coordinates, four scroll directions/arguments, passive/active updates and disconnect removal")
        finally:
            Path(os.environ.get("WM_CHECK_TRAY_UI_LOG", "/tmp/luma-tray-ui.log")).write_text(
                log_path.read_text(errors="replace")
            )
            if process.poll() is None:
                nested.request(socket, "quit")
                process.wait(timeout=5)
