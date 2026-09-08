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


def move_pointer(window, x, y):
    """Wait for the host X11 pointer warp, independently of rendered pixels."""
    deadline = time.monotonic() + 5
    while True:
        window_arg = str(window)
        subprocess.run(["xdotool", "mousemove", "--window", window_arg, str(x - 1), str(y - 1),
                        "mousemove", "--window", window_arg, str(x), str(y)], check=True, timeout=5)
        time.sleep(.03)
        def fields(args):
            return dict(line.split("=", 1) for line in subprocess.check_output(args, text=True).splitlines() if "=" in line)
        geometry = fields(["xdotool", "getwindowgeometry", "--shell", window_arg])
        pointer = fields(["xdotool", "getmouselocation", "--shell"])
        expected = (int(geometry["X"]) + x, int(geometry["Y"]) + y)
        actual = (int(pointer["X"]), int(pointer["Y"]))
        if actual == expected:
            return
        assert time.monotonic() < deadline, ("host pointer warp did not arrive", expected, actual)


def check_closing(config, motion_config, command, capture, path, process, directory, log_path, remap_binary, x11_binary, sample_point, wid):
    motion_config = motion_config.replace("[theme]", "[theme]\nradius=24\nborder=0\nshadow_size=0")
    config.write_text(motion_config)
    command("reload")
    closing_image = capture("closing-before")
    closing_before = closing_image.getpixel(sample_point)
    rect = command("status")["windows"][0]["geometry"]
    corner_point = (rect["x"] + 1, rect["y"] + 1)
    corner_before = closing_image.getpixel(corner_point)
    command("close")
    nested.wait_for(path, lambda s: not s["windows"], process)
    closing_colors = []
    for index in range(4):
        frame = capture("closing-frame-" + str(index))
        closing_colors.append(frame.getpixel(sample_point))
        assert frame.getpixel(corner_point) == corner_before, ("closing corner changed", corner_before, frame.getpixel(corner_point))
        time.sleep(.15)
    time.sleep(2)
    completed = capture("closing-complete")
    closing_background = completed.getpixel(sample_point)
    assert completed.getpixel(corner_point) == corner_before, "rounded corner did not reveal the background"
    distances = [sum(abs(a-b) for a,b in zip(color, closing_background)) for color in closing_colors]
    assert len(set(closing_colors)) >= 3 and distances[0] > distances[-1], (closing_before, closing_colors, closing_background)
    assert distances == sorted(distances, reverse=True), distances
    command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "python3", str(ROOT / "tools/interaction-client.py")]))
    nested.wait_for(path, lambda s: bool(s["windows"]), process)
    time.sleep(2.1)
    command("close")
    nested.wait_for(path, lambda s: not s["windows"], process)
    command("workspace 2")
    assert capture("closing-workspace-cleared").getpixel(sample_point) == closing_background
    command("workspace 1")
    config.write_text(motion_config.replace("animation_ms=2000", "animation_ms=2000\nreduced_motion=true"))
    command("reload")
    command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "python3", str(ROOT / "tools/interaction-client.py")]))
    nested.wait_for(path, lambda s: bool(s["windows"]), process)
    command("close")
    nested.wait_for(path, lambda s: not s["windows"], process)
    assert capture("closing-reduced").getpixel(sample_point) == closing_background
    x11_wallpaper = directory / "x11-close-wallpaper.png"
    Image.new("RGB", (8, 8), (32, 64, 96)).save(x11_wallpaper)
    x11_output = command("status")["outputs"][0]["name"]
    x11_config = '[theme]\nblur=false\nradius=24\nborder=0\nshadow_size=0\nanimation_ms=2000\n'
    x11_config += '[outputs.' + json.dumps(x11_output) + ']\nscale=1.0\n'
    x11_config += '[wallpaper]\npath=' + json.dumps(str(x11_wallpaper)) + '\n'
    x11_config += '[[rules]]\napp_id="org.customwm.X11Close"\nfloating=true\nwidth=360\nheight=220\n'
    config.write_text(x11_config)
    command("reload")
    time.sleep(.2)
    x11_background = capture("x11-close-background")
    command("exec " + json.dumps([str(x11_binary)]))
    x11_state = nested.wait_for(
        path,
        lambda state: any(window["app_id"] == "org.customwm.X11Close" for window in state["windows"]),
        process,
    )
    time.sleep(2.1)
    x11_window = next(window for window in x11_state["windows"] if window["app_id"] == "org.customwm.X11Close")
    x11_target = command("floating")["windows"][0]["geometry"]
    assert (x11_window["geometry"]["w"], x11_window["geometry"]["h"]) != (x11_target["w"], x11_target["h"])
    time.sleep(.25)
    x11_live = capture("x11-close-live")
    x11_mask = ImageChops.difference(
        x11_live, Image.new("RGB", x11_live.size, (208, 64, 64))
    ).convert("L").point(lambda value: 255 if value == 0 else 0)
    x11_bounds = x11_mask.getbbox()
    assert x11_bounds, "resizing XWayland window disappeared"
    x11_size = (x11_bounds[2] - x11_bounds[0], x11_bounds[3] - x11_bounds[1])
    assert min(x11_window["geometry"]["w"], x11_target["w"]) < x11_size[0] < max(x11_window["geometry"]["w"], x11_target["w"]), x11_size
    assert min(x11_window["geometry"]["h"], x11_target["h"]) < x11_size[1] < max(x11_window["geometry"]["h"], x11_target["h"]), x11_size
    x11_point = (
        (x11_bounds[0] + x11_bounds[2]) // 2,
        (x11_bounds[1] + x11_bounds[3]) // 2,
    )
    assert x11_live.getpixel(x11_point) == (208, 64, 64)
    command("close")
    nested.wait_for(path, lambda state: not state["windows"], process)
    x11_closing = capture("x11-close-frame").getpixel(x11_point)
    x11_base = x11_background.getpixel(x11_point)
    x11_delta = [target - base for target, base in zip((208, 64, 64), x11_base)]
    x11_observed = [actual - base for actual, base in zip(x11_closing, x11_base)]
    x11_alpha = sum(a * b for a, b in zip(x11_delta, x11_observed)) / sum(a * a for a in x11_delta)
    x11_error = max(abs(a - b * x11_alpha) for a, b in zip(x11_observed, x11_delta))
    assert 0 < x11_alpha <= 1 and x11_error <= 3, (x11_closing, x11_base, x11_alpha, x11_error)
    time.sleep(2.1)
    assert capture("x11-close-expired").getpixel(x11_point) == x11_base
    print("PASS: XWayland resize interpolates on the GPU and a mid-resize close retains live pixels through expiry")
    config.write_text(motion_config)
    command("reload")
    remap_command = directory / "remap-command"
    remap_receipt = directory / "remap-receipt"
    command("exec " + json.dumps(["env", "WM_REMAP_TEST=1", str(remap_binary), str(remap_command), str(remap_receipt)]))
    nested.wait_for(path, lambda s: bool(s["windows"]), process)
    time.sleep(2.1)
    assert capture("remap-before").getpixel(sample_point) == (192,128,64)
    def remap(action):
        remap_command.write_text(action)
        deadline = time.monotonic() + 5
        while not remap_receipt.exists() or not remap_receipt.read_text().startswith(action + " "):
            assert time.monotonic() < deadline, (action, log_path.read_text(errors="replace")[-2000:])
            time.sleep(.02)
    remap("hide")
    assert capture("remap-hidden-fade").getpixel(sample_point)[0] > closing_background[0] + 20
    hidden_commits = int(remap_receipt.read_text().split()[1])
    remap("show")
    assert int(remap_receipt.read_text().split()[1]) > hidden_commits, "remapping did not commit a new buffer"
    remapped = capture("remap-new-color").getpixel(sample_point)
    remap_deadline = time.monotonic() + 1
    while remapped[1] <= closing_background[1] + 20:
        assert time.monotonic() < remap_deadline, ("new buffer did not appear", remapped)
        time.sleep(.02)
        remapped = capture("remap-new-color").getpixel(sample_point)
    target = (32, 192, 128)
    delta = [a - b for a, b in zip(target, closing_background)]
    observed = [a - b for a, b in zip(remapped, closing_background)]
    opening_alpha = sum(a * b for a, b in zip(delta, observed)) / sum(a * a for a in delta)
    opening_error = max(abs(a - b * opening_alpha) for a, b in zip(observed, delta))
    assert 0 < opening_alpha <= 1 and opening_error <= 3, (
        "old snapshot covered remapped opening fade",
        remapped,
        closing_background,
        opening_alpha,
        opening_error,
    )
    # Disconnect without destroying any Wayland object explicitly.
    time.sleep(2.1)
    assert capture("remap-opening-complete").getpixel(sample_point) == target
    remap_command.write_text("quit")
    nested.wait_for(path, lambda s: not s["windows"], process)
    disconnected = capture("disconnect-closing").getpixel(sample_point)
    assert disconnected[1] > closing_background[1] + 20, (disconnected, closing_background)
    time.sleep(2.1)
    assert capture("disconnect-expired").getpixel(sample_point) == closing_background
    for decoration_scale in (1.0, 1.5):
        decoration_config = motion_config.replace("border=0", "border=6").replace("shadow_size=0", "shadow_size=16\nshadow_opacity=0.5")
        decoration_config += '\n[[rules]]\napp_id="org.customwm.InteractionTest"\nfloating=true\nwidth=320\nheight=200\n'
        output_name = command("status")["outputs"][0]["name"]
        output_section = '[outputs.' + json.dumps(output_name) + ']'
        if output_section in decoration_config:
            decoration_config = decoration_config.replace("scale=1.0", "scale=" + str(decoration_scale))
        else:
            decoration_config += "\n" + output_section + "\nscale=" + str(decoration_scale) + "\n"
        config.write_text(decoration_config)
        command("reload")
        time.sleep(.2)
        decoration_background = capture("decoration-background-" + str(decoration_scale))
        command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "python3", str(ROOT / "tools/interaction-client.py")]))
        nested.wait_for(path, lambda state: bool(state["windows"]), process)
        time.sleep(2.1)
        rect = command("status")["windows"][0]["geometry"]
        points = [(rect["x"] + 30, rect["y"] + 30),
                  (rect["x"] - 3, rect["y"] + rect["h"] // 2),
                  (rect["x"] - 14, rect["y"] + rect["h"] // 2),
                  (rect["x"] + 7, rect["y"] + 7),
                  (rect["x"] + 10, rect["y"] + 2)]
        points = [(round(x * decoration_scale), round(y * decoration_scale)) for x, y in points]
        before = capture("decoration-before-" + str(decoration_scale))
        foreground = [before.getpixel(point) for point in points]
        background = [decoration_background.getpixel(point) for point in points]
        assert all(a != b for a, b in zip(foreground[:3], background[:3])), ("missing live decoration", foreground, background)
        close_started = time.monotonic()
        command("close")
        nested.wait_for(path, lambda state: not state["windows"], process)
        for index in range(4):
            frame = capture("decoration-fade-" + str(decoration_scale) + "-" + str(index))
            pixels = [frame.getpixel(point) for point in points]
            alpha = (pixels[0][0] - background[0][0]) / (foreground[0][0] - background[0][0])
            if not 0 < alpha < 1:
                before.save("/tmp/luma-closing-before.png")
                frame.save("/tmp/luma-closing-failed.png")
                Path("/tmp/luma-closing-fractional-session.log").write_text(log_path.read_text())
            assert 0 < alpha < 1, (decoration_scale, alpha, time.monotonic() - close_started, rect, points, foreground, background, pixels)
            for actual, start, end in zip(pixels[1:], foreground[1:], background[1:]):
                expected = [round(b + (a - b) * alpha) for a, b in zip(start, end)]
                assert max(abs(a - b) for a, b in zip(actual, expected)) <= 3, ("decoration fade mismatch", decoration_scale, actual, expected, alpha)
            time.sleep(.15)
        time.sleep(2)
        completed = capture("decoration-expired-" + str(decoration_scale))
        assert [completed.getpixel(point) for point in points] == background
    command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "WM_TEST_POPOVER=1", "python3", str(ROOT / "tools/interaction-client.py")]))
    nested.wait_for(path, lambda state: bool(state["windows"]), process)
    time.sleep(2.1)
    rect = command("status")["windows"][0]["geometry"]
    popup_image = capture("popover-outside-parent")
    point = (round((rect["x"] + rect["w"] + 30) * 1.5), round((rect["y"] + rect["h"] / 2) * 1.5))
    if popup_image.getpixel(point) != (32, 192, 128):
        popup_image.save("/tmp/luma-popover-clipping.png")
    assert popup_image.getpixel(point) == (32, 192, 128), ("popover clipped outside parent", point, popup_image.getpixel(point))

    def popup_bounds(frame):
        mask = ImageChops.difference(
            frame, Image.new("RGB", frame.size, (32, 192, 128))
        ).convert("L").point(lambda value: 255 if value == 0 else 0)
        return mask.getbbox()

    popup_start = popup_bounds(popup_image)
    assert popup_start is not None
    command("floating")
    popup_frames = []
    for index in range(8):
        bounds = popup_bounds(capture("popover-resize-" + str(index)))
        assert bounds is not None, "popover disappeared during its parent resize"
        popup_frames.append(bounds)
        time.sleep(.08)
    time.sleep(1.5)
    popup_end = popup_bounds(capture("popover-resize-complete"))
    assert popup_end is not None and popup_end != popup_start, (popup_start, popup_end)
    assert any(bounds != popup_start and bounds != popup_end for bounds in popup_frames), (popup_start, popup_frames, popup_end)
    print("PASS: popup content extends beyond its rounded parent and remains attached during GPU resize")
    command("close")
    nested.wait_for(path, lambda state: not state["windows"], process)
    time.sleep(2.1)
    striped_path = directory / "blur-stripes.png"
    stripes = Image.new("RGB", (960, 640))
    stripes.putdata([(32, 64, 96) if (x // 8) % 2 == 0 else (160, 64, 32) for y in range(640) for x in range(960)])
    stripes.save(striped_path)
    blur_config = '[theme]\nblur=true\nblur_passes=8\nanimation_ms=2000\nradius=24\nborder=0\nshadow_size=0\n'
    blur_config += '[wallpaper]\npath=' + json.dumps(str(striped_path)) + '\n[outputs.' + json.dumps(output_name) + ']\nscale=1.0\n'
    blur_config += '[[rules]]\napp_id="org.customwm.InteractionTest"\nfloating=true\nwidth=320\nheight=200\nopacity=0.5\nblur=true\n'
    config.write_text(blur_config)
    command("reload")
    time.sleep(.3)
    blur_background = capture("blur-background")
    command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "python3", str(ROOT / "tools/interaction-client.py")]))
    nested.wait_for(path, lambda state: bool(state["windows"]), process)
    time.sleep(2.1)
    rect = command("status")["windows"][0]["geometry"]
    points = [(rect["x"] + x, rect["y"] + 40) for x in (40, 48, 56, 64)]
    live = capture("blur-live")
    before = [live.getpixel(point) for point in points]
    background = [blur_background.getpixel(point) for point in points]
    unblurred = [[round((a + b) / 2) for a, b in zip((192, 128, 64), pixel)] for pixel in background]
    assert max(abs(a - b) for actual, expected in zip(before, unblurred) for a, b in zip(actual, expected)) > 12, ("live blur was not observable", before, unblurred)
    command("close")
    nested.wait_for(path, lambda state: not state["windows"], process)
    for index in range(4):
        frame = capture("blur-closing-" + str(index))
        actual = [frame.getpixel(point) for point in points]
        delta = [a - b for pixel, base in zip(before, background) for a, b in zip(pixel, base)]
        observed = [a - b for pixel, base in zip(actual, background) for a, b in zip(pixel, base)]
        alpha = sum(a*b for a,b in zip(delta, observed)) / sum(a*a for a in delta)
        error = max(abs(a - d * alpha) for a, d in zip(observed, delta))
        if error > 4:
            live.save("/tmp/luma-closing-blur-live.png")
            frame.save("/tmp/luma-closing-blur-failed.png")
        assert 0 < alpha < 1 and error <= 4, ("closing blur changed", alpha, error, before, actual, background)
        time.sleep(.15)
    time.sleep(2)
    complete = capture("blur-expired")
    assert [complete.getpixel(point) for point in points] == background
    print("PASS: live background blur is retained through closing and expires completely")
    stack_config = '[theme]\nblur=false\nanimation_ms=2000\nradius=24\nborder=0\nshadow_size=0\n'
    stack_config += '[outputs.' + json.dumps(output_name) + ']\nscale=1.0\n'
    stack_config += '[[rules]]\napp_id="org.customwm.RemapTest"\nfloating=true\nwidth=400\nheight=300\n'
    stack_config += '[[rules]]\napp_id="org.customwm.InteractionTest"\nfloating=true\nwidth=320\nheight=200\n'
    config.write_text(stack_config)
    command("reload")
    time.sleep(.3)
    stack_background = capture("stack-background")
    stack_command = directory / "stack-command"
    stack_receipt = directory / "stack-receipt"
    def open_stack_window():
        stack_command.unlink(missing_ok=True)
        command("exec " + json.dumps(["env", "WM_REMAP_TEST=1", str(remap_binary), str(stack_command), str(stack_receipt)]))
        nested.wait_for(path, lambda state: any(w["app_id"] == "org.customwm.RemapTest" for w in state["windows"]), process)
        time.sleep(2.1)
        return next(w["geometry"] for w in command("status")["windows"] if w["app_id"] == "org.customwm.RemapTest")
    bottom = open_stack_window()
    command("exec " + json.dumps(["env", "WM_TEST_COLOR=#20c080", "python3", str(ROOT / "tools/interaction-client.py")]))
    nested.wait_for(path, lambda state: len(state["windows"]) == 2, process)
    time.sleep(2.1)
    top = next(w["geometry"] for w in command("status")["windows"] if w["app_id"] == "org.customwm.InteractionTest")
    overlap = (top["x"] + 30, top["y"] + 30)
    exposed = (bottom["x"] + 20, bottom["y"] + 40)
    before = capture("stack-live")
    assert before.getpixel(overlap) == (32, 192, 128)
    assert before.getpixel(exposed) == (192, 128, 64)
    stack_command.write_text("quit")
    nested.wait_for(path, lambda state: len(state["windows"]) == 1, process)
    frame = capture("stack-bottom-closing")
    assert frame.getpixel(overlap) == (32, 192, 128), ("closing image jumped above covering window", frame.getpixel(overlap))
    assert frame.getpixel(exposed) != stack_background.getpixel(exposed), "uncovered closing pixels vanished"
    time.sleep(2.1)
    assert capture("stack-bottom-expired").getpixel(exposed) == stack_background.getpixel(exposed)
    open_stack_window()
    assert capture("stack-top-live").getpixel(overlap) == (192, 128, 64)
    stack_command.write_text("quit")
    nested.wait_for(path, lambda state: len(state["windows"]) == 1, process)
    assert capture("stack-top-closing").getpixel(overlap)[0] > 52, "foreground closing image fell behind its lower window"
    live_id = command("status")["windows"][0]["id"]
    command("focus " + str(live_id))
    raised = capture("stack-explicit-raise")
    assert raised.getpixel(overlap) == (32, 192, 128), "explicit focus stayed behind the closing image"
    assert raised.getpixel(exposed) != stack_background.getpixel(exposed), "raising another window cancelled the entire closing image"
    time.sleep(2.1)

    move_pointer(wid, 900, 600)
    open_stack_window()
    assert capture("stack-pointer-top-live").getpixel(overlap) == (192, 128, 64)
    stack_command.write_text("quit")
    nested.wait_for(path, lambda state: len(state["windows"]) == 1, process)
    assert capture("stack-pointer-top-closing").getpixel(overlap)[0] > 52
    move_pointer(wid, *overlap)
    subprocess.run(["xdotool", "click", "1"], check=True, timeout=5)
    deadline = time.monotonic() + 2
    while True:
        pointer_raised = capture("stack-pointer-raise")
        if pointer_raised.getpixel(overlap) == (32, 192, 128):
            break
        assert time.monotonic() < deadline, "pointer activation stayed behind the closing image"
        time.sleep(.03)
    assert pointer_raised.getpixel(exposed) != stack_background.getpixel(exposed), "pointer activation cancelled the entire closing image"
    time.sleep(2.1)

    open_stack_window()
    stack_command.write_text("quit")
    nested.wait_for(path, lambda state: len(state["windows"]) == 1, process)
    assert capture("stack-before-fullscreen").getpixel(overlap)[0] > 52
    command("fullscreen")
    nested.wait_for(path, lambda state: state["windows"][0]["fullscreen"]
        and state["windows"][0].get("surface_size") == [state["windows"][0]["geometry"]["w"], state["windows"][0]["geometry"]["h"]], process)
    fullscreen_pixel = capture("stack-fullscreen").getpixel(overlap)
    assert fullscreen_pixel == (32, 192, 128), ("closing image covered fullscreen content", fullscreen_pixel)
    time.sleep(2.1)
    command("close")
    nested.wait_for(path, lambda state: not state["windows"], process)
    fullscreen_closing = capture("fullscreen-closing")
    edge = (2, fullscreen_closing.height - 3)
    fullscreen_background = stack_background.getpixel(edge)
    assert fullscreen_closing.getpixel(edge)[1] > fullscreen_background[1] + 20, ("fullscreen closing image missing or rounded", fullscreen_closing.getpixel(edge), fullscreen_background)
    time.sleep(2.1)
    assert capture("fullscreen-closing-expired").getpixel(edge) == fullscreen_background
    open_stack_window()
    command("fullscreen")
    nested.wait_for(path, lambda state: state["windows"][0]["fullscreen"]
        and state["windows"][0].get("surface_size") == [state["outputs"][0]["geometry"]["w"], state["outputs"][0]["geometry"]["h"]], process)
    command("exec " + json.dumps(["env", "WM_TEST_COLOR=#20c080", "python3", str(ROOT / "tools/interaction-client.py")]))
    nested.wait_for(path, lambda state: len(state["windows"]) == 2, process)
    time.sleep(2.1)
    assert capture("fullscreen-with-underlying-window").getpixel(overlap) == (192, 128, 64)
    stack_command.write_text("quit")
    nested.wait_for(path, lambda state: len(state["windows"]) == 1, process)
    assert capture("fullscreen-closing-above-new-window").getpixel(overlap)[0] > 52, "fullscreen snapshot fell behind a previously hidden window"
    time.sleep(2.1)
    assert capture("fullscreen-underlying-revealed").getpixel(overlap) == (32, 192, 128)
    command("close")
    nested.wait_for(path, lambda state: not state["windows"], process)
    print("PASS: fullscreen windows close with unrounded edge pixels and expire completely")
    print("PASS: closing images retain order, respect explicit focus, and stay behind fullscreen content")
    mass_windows = [
        ("org.customwm.Mass1", "#d04040", (840, 560), (208, 64, 64)),
        ("org.customwm.Mass2", "#4060d0", (740, 490), (64, 96, 208)),
        ("org.customwm.Mass3", "#d0c040", (640, 420), (208, 192, 64)),
        ("org.customwm.Mass4", "#40c060", (540, 350), (64, 192, 96)),
        ("org.customwm.Mass5", "#c040c0", (440, 280), (192, 64, 192)),
        ("org.customwm.Mass6", "#40c0d0", (340, 220), (64, 192, 208)),
        ("org.customwm.Mass7", "#d08040", (240, 160), (208, 128, 64)),
        ("org.customwm.Mass8", "#8090d0", (140, 100), (128, 144, 208)),
    ]
    mass_wallpaper = directory / "mass-close-wallpaper.png"
    Image.new("RGB", (8, 8), (32, 64, 96)).save(mass_wallpaper)
    mass_theme = '[theme]\nblur=false\nradius=20\nborder=0\nshadow_size=0\n'
    mass_output = '[outputs.' + json.dumps(output_name) + ']\nscale=1.0\n'
    mass_wallpaper_config = '[wallpaper]\npath=' + json.dumps(str(mass_wallpaper)) + '\n'
    resize_control = directory / "resize-close-command"
    resize_receipt = directory / "resize-close-receipt"
    resize_config = mass_theme + 'animation_ms=2000\n' + mass_output + mass_wallpaper_config
    resize_config += '[[rules]]\napp_id="org.customwm.ResizeClose"\nfloating=true\nwidth=320\nheight=200\n'
    config.write_text(resize_config)
    command("reload")
    time.sleep(.2)
    resize_background = capture("resize-close-background")
    command("exec " + json.dumps([
        "env",
        "WM_REMAP_TEST=1",
        "WM_REMAP_APP_ID=org.customwm.ResizeClose",
        "WM_REMAP_COLOR=#d04040",
        str(remap_binary),
        str(resize_control),
        str(resize_receipt),
    ]))
    nested.wait_for(path, lambda state: len(state["windows"]) == 1, process)
    time.sleep(2.1)
    initial_resize = command("status")["windows"][0]["geometry"]
    resize_target = command("floating")["windows"][0]["geometry"]
    assert (initial_resize["w"], initial_resize["h"]) != (resize_target["w"], resize_target["h"])
    time.sleep(.25)
    resize_mid = capture("resize-close-mid")
    resize_mask = ImageChops.difference(
        resize_mid, Image.new("RGB", resize_mid.size, (208, 64, 64))
    ).convert("L").point(lambda value: 255 if value == 0 else 0)
    resize_mid_bounds = resize_mask.getbbox()
    assert resize_mid_bounds, "resizing window disappeared"
    resize_mid_size = (
        resize_mid_bounds[2] - resize_mid_bounds[0],
        resize_mid_bounds[3] - resize_mid_bounds[1],
    )
    assert min(initial_resize["w"], resize_target["w"]) < resize_mid_size[0] < max(initial_resize["w"], resize_target["w"]), resize_mid_size
    assert min(initial_resize["h"], resize_target["h"]) < resize_mid_size[1] < max(initial_resize["h"], resize_target["h"]), resize_mid_size
    resize_control.write_text("quit")
    nested.wait_for(path, lambda state: not state["windows"], process)
    def red_blend_mask(frame):
        mask = Image.new("L", frame.size)
        pixels = []
        for actual, base in zip(frame.get_flattened_data(), resize_background.get_flattened_data()):
            delta = [target - origin for target, origin in zip((208, 64, 64), base)]
            observed = [value - origin for value, origin in zip(actual, base)]
            denominator = sum(value * value for value in delta)
            alpha = sum(a * b for a, b in zip(delta, observed)) / denominator if denominator else 0
            error = max(abs(a - b * alpha) for a, b in zip(observed, delta))
            pixels.append(255 if .02 < alpha <= 1.02 and error <= 3 else 0)
        mask.putdata(pixels)
        return mask

    resize_closing = capture("resize-close-snapshot")
    resize_blend = red_blend_mask(resize_closing)
    resize_region = (
        max(0, resize_mid_bounds[0] - 50),
        max(0, resize_mid_bounds[1] - 50),
        min(resize_blend.width, resize_mid_bounds[2] + 50),
        min(resize_blend.height, resize_mid_bounds[3] + 50),
    )
    resize_closing_local = resize_blend.crop(resize_region).getbbox()
    resize_closing_bounds = None if resize_closing_local is None else (
        resize_closing_local[0] + resize_region[0],
        resize_closing_local[1] + resize_region[1],
        resize_closing_local[2] + resize_region[0],
        resize_closing_local[3] + resize_region[1],
    )
    assert resize_closing_bounds, "mid-resize closing snapshot disappeared"
    assert all(abs(a - b) <= 25 for a, b in zip(resize_mid_bounds, resize_closing_bounds)), (
        "mid-resize close jumped geometry",
        resize_mid_bounds,
        resize_closing_bounds,
    )
    expiry_deadline = time.monotonic() + 3
    while True:
        expiry_frame = capture("resize-close-expired")
        expiry_mask = red_blend_mask(expiry_frame).crop(resize_region)
        expired = expiry_mask.getbbox()
        if expired is None:
            break
        if time.monotonic() >= expiry_deadline:
            expiry_frame.save("/tmp/luma-resize-close-expired.png")
            expiry_mask.save("/tmp/luma-resize-close-expired-mask.png")
            raise AssertionError(("mid-resize closing snapshot did not expire", expired))
        time.sleep(.03)
    print("PASS: GPU resize interpolation survives a mid-animation disconnect without a geometry jump")
    # Disable animations for one policy tick so no fade from the preceding
    # fullscreen scenario can contaminate this test's background baseline.
    config.write_text(mass_theme + 'animation_ms=0\n' + mass_output + mass_wallpaper_config)
    command("reload")
    time.sleep(.2)
    mass_config = mass_theme + 'animation_ms=2000\n' + mass_output + mass_wallpaper_config
    for app_id, _color, size, _target in mass_windows:
        mass_config += (
            '[[rules]]\napp_id=' + json.dumps(app_id) + '\nfloating=true\n'
            + 'width=' + str(size[0]) + '\nheight=' + str(size[1]) + '\n'
        )
    config.write_text(mass_config)
    command("reload")
    time.sleep(.3)
    mass_background = capture("mass-close-background")
    mass_controls = []
    for index, (app_id, color, _size, _target) in enumerate(mass_windows):
        control = directory / ("mass-command-" + str(index))
        receipt = directory / ("mass-receipt-" + str(index))
        command("exec " + json.dumps([
            "env",
            "WM_REMAP_TEST=1",
            "WM_REMAP_APP_ID=" + app_id,
            "WM_REMAP_COLOR=" + color,
            str(remap_binary),
            str(control),
            str(receipt),
        ]))
        nested.wait_for(path, lambda state, count=index + 1: len(state["windows"]) == count, process)
        mass_controls.append(control)
    time.sleep(2.1)
    geometry = {
        window["app_id"]: window["geometry"]
        for window in command("status")["windows"]
    }

    def exposed_point(rect, covers):
        for y in range(rect["y"] + 30, rect["y"] + rect["h"] - 30, 10):
            for x in range(rect["x"] + 30, rect["x"] + rect["w"] - 30, 10):
                if not any(
                    cover["x"] <= x < cover["x"] + cover["w"]
                    and cover["y"] <= y < cover["y"] + cover["h"]
                    for cover in covers
                ):
                    return (x, y)
        raise AssertionError(("no exposed sample for mass-close window", rect, covers))

    points = []
    for index, (app_id, _color, _size, _target) in enumerate(mass_windows):
        covers = [geometry[higher[0]] for higher in mass_windows[index + 1:]]
        points.append(exposed_point(geometry[app_id], covers))
    live = capture("mass-close-live")
    for point, (_app_id, _color, _size, target) in zip(points, mass_windows):
        assert live.getpixel(point) == target, ("mass-close live stacking", point, live.getpixel(point), target)
    for control in mass_controls:
        control.write_text("quit")
    nested.wait_for(path, lambda state: not state["windows"], process)
    closing = capture("mass-close-frame")
    measured_alpha = []
    for index, (point, (_app_id, _color, _size, target)) in enumerate(zip(points, mass_windows)):
        background = list(mass_background.getpixel(point))
        # The sample is exposed from every higher window, but lower closing
        # windows may still sit beneath it. Reconstruct that base in z-order.
        for lower in range(index):
            lower_rect = geometry[mass_windows[lower][0]]
            if (
                lower_rect["x"] <= point[0] < lower_rect["x"] + lower_rect["w"]
                and lower_rect["y"] <= point[1] < lower_rect["y"] + lower_rect["h"]
            ):
                lower_target = mass_windows[lower][3]
                lower_alpha = measured_alpha[lower]
                background = [
                    round(base + (value - base) * lower_alpha)
                    for base, value in zip(background, lower_target)
                ]
        actual = closing.getpixel(point)
        delta = [a - b for a, b in zip(target, background)]
        observed = [a - b for a, b in zip(actual, background)]
        alpha = sum(a * b for a, b in zip(delta, observed)) / sum(a * a for a in delta)
        error = max(abs(a - b * alpha) for a, b in zip(observed, delta))
        measured_alpha.append(alpha)
        assert 0 < alpha <= 1 and error <= 3, (
            "simultaneous closing image missing or reordered",
            point,
            actual,
            target,
            background,
            alpha,
            error,
        )
    expiry_deadline = time.monotonic() + 3
    while True:
        expired = capture("mass-close-expired")
        remaining = [
            (point, expired.getpixel(point), mass_background.getpixel(point))
            for point in points
            if expired.getpixel(point) != mass_background.getpixel(point)
        ]
        if not remaining:
            break
        assert time.monotonic() < expiry_deadline, ("simultaneous closing images did not expire", remaining)
        time.sleep(.02)
    print("PASS: eight simultaneous closing images retain their exposed pixels, order, and expiry")
    print("PASS: GPU closing fade, rounded corners and border/shadow parity at 1x/1.5x, remap cancellation, abrupt disconnect, expiry, workspace isolation and reduced-motion bypass")


def check_fullscreen_remap(config, command, capture, path, process, directory, remap_binary):
    config.write_text('[theme]\nblur=false\nanimation_ms=0\n[[rules]]\napp_id="org.customwm.RemapTest"\nfloating=true\nwidth=320\nheight=200\n')
    command("reload")
    control = directory / "fullscreen-remap-command"
    receipt = directory / "fullscreen-remap-receipt"
    command("exec " + json.dumps(["env", "WM_REMAP_TEST=1", str(remap_binary), str(control), str(receipt)]))
    nested.wait_for(path, lambda state: len(state["windows"]) == 1 and state["windows"][0].get("surface_size") == [320, 200], process)
    command("fullscreen")
    nested.wait_for(path, lambda state: state["windows"][0]["fullscreen"] and state["windows"][0].get("surface_size") == [state["outputs"][0]["geometry"]["w"], state["outputs"][0]["geometry"]["h"]], process)
    def change(action):
        control.write_text(action)
        deadline = time.monotonic() + 5
        while not receipt.exists() or not receipt.read_text().startswith(action + " "):
            assert time.monotonic() < deadline, ("fullscreen remap command not acknowledged", action)
            time.sleep(.02)
    change("hide")
    state = command("status")
    assert all(not window["fullscreen"] for window in state["windows"]), ("unmapped xdg toplevel retained fullscreen", state["windows"])
    change("show")
    state = nested.wait_for(path, lambda state: len(state["windows"]) == 1 and not state["windows"][0]["fullscreen"] and state["windows"][0].get("surface_size") == [320, 200], process)
    rect = state["windows"][0]["geometry"]
    assert (rect["w"], rect["h"]) == (320, 200), rect
    assert capture("fullscreen-remapped-normal").getpixel((rect["x"] + 30, rect["y"] + 30)) == (32, 192, 128)
    control.write_text("quit")
    nested.wait_for(path, lambda state: not state["windows"], process)
    print("PASS: fullscreen unmap resets role state and remaps at the normal floating size")


def main():
    native_wayland = bool(os.environ.get("WM_CAPTURE_NATIVE_WAYLAND"))
    if native_wayland and not os.environ.get("WM_CHECK_CLOSING_ONLY"):
        raise RuntimeError("native Wayland capture currently supports the closing-only fixture; full input/resize checks require X11")
    with tempfile.TemporaryDirectory(prefix="wm-capture-") as temp:
        directory = Path(temp)
        remap_protocol = Path(subprocess.check_output(["pkg-config", "--variable=pkgdatadir", "wayland-protocols"], text=True).strip()) / "stable/xdg-shell/xdg-shell.xml"
        subprocess.run(["wayland-scanner", "client-header", str(remap_protocol), str(directory / "xdg-shell-client-protocol.h")], check=True)
        subprocess.run(["wayland-scanner", "private-code", str(remap_protocol), str(directory / "xdg-shell-protocol.c")], check=True)
        remap_binary = directory / "remap-client"
        remap_flags = subprocess.check_output(["pkg-config", "--cflags", "--libs", "wayland-client"], text=True).split()
        subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-I", str(directory), str(ROOT / "tools/remap-client.c"), str(directory / "xdg-shell-protocol.c"), "-o", str(remap_binary), *remap_flags], check=True)
        x11_binary = directory / "x11-client"
        x11_flags = subprocess.check_output(["pkg-config", "--cflags", "--libs", "xcb"], text=True).split()
        subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", str(ROOT / "tools/x11-client.c"), "-o", str(x11_binary), *x11_flags], check=True)

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
            capture_serial = 0
            def capture(name, resize=False, cursor=False):
                nonlocal capture_serial
                capture_serial += 1
                # Output geometry changes before the wallpaper client can
                # commit a resized buffer. Observe that commit independently
                # of the pixels this test will subsequently validate.
                def wallpaper_ready(state):
                    outputs = {output["name"]: output["geometry"] for output in state["outputs"]}
                    layers = [layer for layer in state["layers"] if layer["namespace"] == "wm-wallpaper"]
                    return bool(layers) and all(
                        layer.get("surface_size") == [outputs[layer["output"]]["w"], outputs[layer["output"]]["h"]]
                        for layer in layers if layer["output"] in outputs)
                nested.wait_for(path, wallpaper_ready, process)
                # Every request needs a fresh completion receipt, including
                # retries that deliberately reuse a descriptive capture name.
                image_path = directory / (str(capture_serial) + "-" + name + ".ppm")
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
            def capture_cursor_pair(name, position=(200, 200), expect_cursor=True):
                # A cursor comparison is only meaningful while the underlying
                # scene is unchanged. Bracket it with cursor-free captures.
                deadline = time.monotonic() + 5
                attempts = 0
                while True:
                    attempts += 1
                    move_pointer(wid, *position)
                    plain = capture(name + "-plain")
                    cursor = capture(name + "-cursor", cursor=True)
                    after = capture(name + "-after")
                    cursor_bounds = ImageChops.difference(plain, cursor).getbbox()
                    if (plain.size == cursor.size == after.size
                            and ImageChops.difference(plain, after).getbbox() is None
                            and (not expect_cursor or cursor_bounds is not None)):
                        if attempts > 1:
                            print("Cursor scene settled:", name, "after", attempts, "attempts")
                        return plain, cursor
                    if time.monotonic() >= deadline:
                        plain.save("/tmp/luma-cursor-unstable-before.png")
                        after.save("/tmp/luma-cursor-unstable-after.png")
                        raise AssertionError(("cursor comparison scene did not settle", name))
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
                resized_image = image if native_wayland else capture("resized", resize=True)
                if not native_wayland:
                    assert resized_image.size != image.size, (resized_image.size, image.size)
                assert resized_image.getpixel((20, resized_image.height // 4)) == (32, 64, 96)
                assert resized_image.getpixel((20, resized_image.height * 3 // 4)) == (160, 64, 32)
                base_config = config.read_text()
                if os.environ.get("WM_CHECK_FULLSCREEN_REMAP_ONLY"):
                    check_fullscreen_remap(config, command, capture, path, process, directory, remap_binary)
                    return
                if os.environ.get("WM_CHECK_CLOSING_ONLY"):
                    motion_config = base_config.replace("animation_ms=0", "animation_ms=2000")
                    config.write_text(motion_config)
                    command("reload")
                    command("exec " + json.dumps(["env", "WM_TEST_COLOR=#c08040", "python3", str(ROOT / "tools/interaction-client.py")]))
                    nested.wait_for(path, lambda state: bool(state["windows"]), process)
                    time.sleep(2.1)
                    rect = command("status")["windows"][0]["geometry"]
                    sample_point = (rect["x"] + 30, rect["y"] + 30)
                    wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
                    check_closing(config, motion_config, command, capture, path, process, directory, log_path, remap_binary, x11_binary, sample_point, wid)
                    return

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
                nested.wait_for(path, lambda s: (s["outputs"][0]["geometry"]["w"], s["outputs"][0]["geometry"]["h"]) == resized_image.size, process)
                assert capture("normal-restored").size == resized_image.size
                print("PASS: upright SHM capture dimensions and pixels across all output rotations/reflections")
                wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
                subprocess.run(["xdotool", "windowfocus" if os.environ.get("WM_CAPTURE_PRIVATE_X11") else "windowactivate", "--sync", wid], check=True, timeout=5)
                move_pointer(wid, 200, 200)
                time.sleep(.2)
                without_cursor, with_cursor = capture_cursor_pair("base")
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
                rotated_window, rotated_cursor = capture_cursor_pair("rotated")
                cursor_bounds = ImageChops.difference(rotated_window, rotated_cursor).getbbox()
                assert cursor_bounds is not None and (cursor_bounds[2] - cursor_bounds[0], cursor_bounds[3] - cursor_bounds[1]) == (16, 20), cursor_bounds
                assert rotated_cursor.getpixel((cursor_bounds[0] + 2, cursor_bounds[1] + 2)) == (0, 255, 128)

                transformed_motion = (base_config.replace("animation_ms=0", "animation_ms=2000")
                                      + f'\n[outputs.{json.dumps(output_name)}]\ntransform="90"\n')
                config.write_text(transformed_motion)
                command("reload")
                time.sleep(.2)

                def client_bounds(frame):
                    mask = ImageChops.difference(
                        frame, Image.new("RGB", frame.size, (192, 128, 64))
                    ).convert("L").point(lambda value: 255 if value == 0 else 0)
                    return mask.getbbox()

                resize_start = client_bounds(capture("transformed-resize-start"))
                assert resize_start is not None
                command("floating")
                resize_frames = []
                for index in range(8):
                    bounds = client_bounds(capture("transformed-resize-frame-" + str(index)))
                    assert bounds is not None, "transformed resize dropped the client image"
                    resize_frames.append((bounds[2] - bounds[0], bounds[3] - bounds[1]))
                    time.sleep(.08)
                time.sleep(1.5)
                resize_end = client_bounds(capture("transformed-resize-end"))
                assert resize_end is not None
                start_size = (resize_start[2] - resize_start[0], resize_start[3] - resize_start[1])
                end_size = (resize_end[2] - resize_end[0], resize_end[3] - resize_end[1])
                assert start_size != end_size, (start_size, end_size)
                assert any(
                    min(start_size[0], end_size[0]) + 3 < size[0] < max(start_size[0], end_size[0]) - 3
                    or min(start_size[1], end_size[1]) + 3 < size[1] < max(start_size[1], end_size[1]) - 3
                    for size in resize_frames
                ), (start_size, resize_frames, end_size)
                command("floating")
                time.sleep(2.1)
                config.write_text(base_config + f'\n[outputs.{json.dumps(output_name)}]\ntransform="normal"\n')
                command("reload")
                nested.wait_for(path, lambda s: s["outputs"][0]["geometry"]["w"] == resized_image.width, process)
                config.write_text(base_config)
                time.sleep(.2)
                move_pointer(wid, 200, 200)
                time.sleep(.2)
                surface_plain, surface_cursor = capture_cursor_pair("surface")
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
                text_plain, text_cursor = capture_cursor_pair("text")
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
                move_pointer(wid, 200, 200)
                time.sleep(.3)
                hidden_plain, hidden_cursor = capture_cursor_pair("hidden", expect_cursor=False)
                assert ImageChops.difference(hidden_plain, hidden_cursor).getbbox() is None, "hidden cursor leaked into capture"
                command("close")
                state = nested.wait_for(path, lambda s: not s["windows"], process)
                with config.open("a") as file:
                    file.write("\n[outputs." + json.dumps(state["outputs"][0]["name"]) + "]\nscale=1.5\n")
                command("reload")
                command("reload")
                nested.wait_for(path, lambda s: abs(s["outputs"][0]["geometry"]["w"] * 1.5 - resized_image.width) <= 1, process)
                pointer_receipt = directory / "scaled-pointer.json"
                command("exec " + json.dumps(["env", "WM_TEST_APP_ID=org.customwm.CaptureScaled", "WM_TEST_POINTER_RECEIPT=" + str(pointer_receipt), "WM_TEST_COLOR=#c08040", "WM_TEST_CURSOR=" + str(cursor_image), "python3", str(ROOT / "tools/interaction-client.py")]))
                nested.wait_for(path, lambda s: bool(s["windows"]), process)
                move_pointer(wid, 201, 201)
                pointer_deadline = time.monotonic() + 5
                while True:
                    state = command("status")
                    rect = state["windows"][0]["geometry"]
                    received = json.loads(pointer_receipt.read_text()) if pointer_receipt.exists() else {}
                    expected = (201 / 1.5 - rect["x"], 201 / 1.5 - rect["y"])
                    if (received.get("x") is not None and received.get("y") is not None
                            and abs(received["x"] - expected[0]) <= 1
                            and abs(received["y"] - expected[1]) <= 1
                            and received["width"] == rect["w"] and received["height"] == rect["h"]):
                        break
                    assert time.monotonic() < pointer_deadline, ("scaled client did not receive the intended pointer event", received, expected, rect)
                    move_pointer(wid, 201, 201)
                    time.sleep(.02)
                scaled_plain, scaled_cursor = capture_cursor_pair("scaled", (201, 201))
                scaled_bounds = ImageChops.difference(scaled_plain, scaled_cursor).getbbox()
                assert scaled_bounds is not None
                if os.environ.get("WM_CAPTURE_DEBUG"):
                    scaled_cursor.save(os.environ["WM_CAPTURE_DEBUG"])
                assert scaled_cursor.getpixel((201, 201)) == (0, 255, 128), (scaled_cursor.getpixel((201, 201)), scaled_bounds, scaled_cursor.size)
                assert 23 <= scaled_bounds[2] - scaled_bounds[0] <= 25 and 29 <= scaled_bounds[3] - scaled_bounds[1] <= 31, scaled_bounds
                assert abs(scaled_bounds[0] - (201 - 7 * 1.5)) <= 1 and abs(scaled_bounds[1] - (201 - 9 * 1.5)) <= 1, scaled_bounds
                output_name = command("status")["outputs"][0]["name"]
                for transform in ("90", "180", "270", "flipped", "flipped-90", "flipped-180", "flipped-270"):
                    config.write_text(base_config + f'\n[outputs.{json.dumps(output_name)}]\nscale=1.5\ntransform={json.dumps(transform)}\n')
                    command("reload")
                    expected = resized_image.size[::-1] if transform in ("90", "270", "flipped-90", "flipped-270") else resized_image.size
                    state = nested.wait_for(path, lambda s: abs(s["outputs"][0]["geometry"]["w"] * 1.5 - expected[0]) <= 1, process)
                    native_w, native_h = resized_image.size
                    cursor_x, cursor_y = {
                        "90": (native_h - 201, 201), "180": (native_w - 201, native_h - 201),
                        "270": (201, native_w - 201), "flipped": (native_w - 201, 201),
                        "flipped-90": (201, 201), "flipped-180": (201, native_h - 201),
                        "flipped-270": (native_h - 201, native_w - 201),
                    }[transform]
                    move_pointer(wid, 201, 201)
                    deadline = time.monotonic() + 5
                    while True:
                        state = command("status")
                        rect = state["windows"][0]["geometry"]
                        received = json.loads(pointer_receipt.read_text()) if pointer_receipt.exists() else {}
                        expected_pointer = (cursor_x / 1.5 - rect["x"], cursor_y / 1.5 - rect["y"])
                        if (received.get("x") is not None and received.get("y") is not None
                                and abs(received["x"] - expected_pointer[0]) <= 1
                                and abs(received["y"] - expected_pointer[1]) <= 1
                                and received["width"] == rect["w"] and received["height"] == rect["h"]):
                            break
                        assert time.monotonic() < deadline, ("transformed pointer/configure not received", transform, expected_pointer, received, rect)
                        move_pointer(wid, 201, 201)
                        time.sleep(.02)
                    render_deadline = time.monotonic() + 5
                    while True:
                        plain, cursor_frame = capture_cursor_pair("scaled-transform-" + transform, (201, 201))
                        state = command("status")
                        rect = state["windows"][0]["geometry"]
                        window_point = (
                            round((rect["x"] + 30) * 1.5),
                            round((rect["y"] + 30) * 1.5),
                        )
                        if plain.size == expected and plain.getpixel(window_point) == (192, 128, 64):
                            break
                        assert time.monotonic() < render_deadline, (
                            "transformed window did not reach reported geometry",
                            transform,
                            plain.size,
                            expected,
                            rect,
                            plain.getpixel(window_point),
                        )
                        time.sleep(.02)
                    bounds = ImageChops.difference(plain, cursor_frame).getbbox()
                    if bounds is None or not (23 <= bounds[2] - bounds[0] <= 25 and 29 <= bounds[3] - bounds[1] <= 31):
                        plain.save("/tmp/luma-cursor-failure-plain.png")
                        cursor_frame.save("/tmp/luma-cursor-failure-cursor.png")
                        capture("cursor-failure-after").save("/tmp/luma-cursor-failure-after.png")
                        Path("/tmp/luma-cursor-failure-state.json").write_text(json.dumps(command("status")))
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
                assert (target["w"], target["h"]) != (old_rect["w"], old_rect["h"]), (old_rect, target)
                positions = []
                sizes = []
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline:
                    frame = capture("movement-" + str(len(positions)))
                    delta = ImageChops.difference(frame, Image.new("RGB", frame.size, (192, 128, 64)))
                    mask = delta.convert("L").point(lambda value: 255 if value == 0 else 0)
                    bounds = mask.getbbox()
                    assert bounds, "moving window disappeared"
                    positions.append(bounds[0])
                    sizes.append((bounds[2] - bounds[0], bounds[3] - bounds[1]))
                    if (abs(bounds[0] - target["x"]) <= 1
                            and abs(sizes[-1][0] - target["w"]) <= 2
                            and abs(sizes[-1][1] - target["h"]) <= 2):
                        break
                assert any(min(old_rect["x"], target["x"]) + 2 < x < max(old_rect["x"], target["x"]) - 2 for x in positions), positions
                assert abs(positions[-1] - target["x"]) <= 1, (positions, target)
                assert any(min(old_rect["w"], target["w"]) + 3 < width < max(old_rect["w"], target["w"]) - 3 for width, _ in sizes), sizes
                assert any(min(old_rect["h"], target["h"]) + 3 < height < max(old_rect["h"], target["h"]) - 3 for _, height in sizes), sizes
                assert abs(sizes[-1][0] - target["w"]) <= 2 and abs(sizes[-1][1] - target["h"]) <= 2, (sizes, target)
                animated_target_size = (target["w"], target["h"])
                config.write_text(motion_config.replace("animation_ms=2000", "animation_ms=2000\nreduced_motion=true"))
                command("reload")
                target = command("floating")["windows"][0]["geometry"]
                reduced_sizes = []
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    frame = capture("movement-reduced-" + str(len(reduced_sizes)))
                    delta = ImageChops.difference(frame, Image.new("RGB", frame.size, (192, 128, 64)))
                    bounds = delta.convert("L").point(lambda value: 255 if value == 0 else 0).getbbox()
                    assert bounds and abs(bounds[0] - target["x"]) <= 1, (bounds, target)
                    size = (bounds[2] - bounds[0], bounds[3] - bounds[1])
                    reduced_sizes.append(size)
                    if abs(size[0] - target["w"]) <= 2 and abs(size[1] - target["h"]) <= 2:
                        break
                    time.sleep(.02)
                assert abs(reduced_sizes[-1][0] - target["w"]) <= 2, (reduced_sizes, target)
                assert abs(reduced_sizes[-1][1] - target["h"]) <= 2, (reduced_sizes, target)
                assert not any(
                    min(animated_target_size[0], target["w"]) + 3 < width < max(animated_target_size[0], target["w"]) - 3
                    and min(animated_target_size[1], target["h"]) + 3 < height < max(animated_target_size[1], target["h"]) - 3
                    for width, height in reduced_sizes
                ), reduced_sizes
                config.write_text(motion_config)
                command("reload")
                command("workspace 2")
                sample_point = (target["x"] + 30, target["y"] + 30)
                outgoing = []
                outgoing_deadline = time.monotonic() + 2.2
                while time.monotonic() < outgoing_deadline:
                    outgoing.append(capture("workspace-leave-" + str(len(outgoing))).getpixel(sample_point))
                    time.sleep(.04)
                empty = capture("workspace-empty")
                assert empty.getpixel(sample_point) != (192, 128, 64), "outgoing workspace still rendered"
                assert any(color != empty.getpixel(sample_point) for color in outgoing), outgoing
                assert any(color not in (empty.getpixel(sample_point), (192, 128, 64)) for color in outgoing), outgoing
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
                assert capture("workspace-leave-reduced").getpixel(sample_point) == empty.getpixel(sample_point)
                command("workspace 1")
                assert capture("workspace-reduced").getpixel(sample_point) == (192, 128, 64)
                check_closing(config, motion_config, command, capture, path, process, directory, log_path, remap_binary, x11_binary, sample_point, wid)
                check_fullscreen_remap(config, command, capture, path, process, directory, remap_binary)
                print("PASS: capture resize recovery, cursor shapes/hiding/scaling and stationary focus, movement/workspace fades and reduced motion")
            except Exception:
                diagnostics = {"configuration": config.read_text(), "log": log_path.read_text(errors="replace")}
                try:
                    diagnostics["state"] = command("status")
                    if os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
                        clients = json.loads(subprocess.check_output(["hyprctl", "-j", "clients"], text=True))
                        diagnostics["host_windows"] = [client for client in clients if client["title"] == title]
                    if "wid" in locals():
                        diagnostics["x11_geometry"] = subprocess.check_output(["xdotool", "getwindowgeometry", "--shell", wid], text=True)
                except Exception as error:
                    diagnostics["diagnostic_error"] = str(error)
                Path("/tmp/luma-capture-failure.json").write_text(json.dumps(diagnostics, indent=2))
                raise
            finally:
                if process.poll() is None:
                    command("quit")
                    process.wait(timeout=5)


if __name__ == "__main__":
    main()
