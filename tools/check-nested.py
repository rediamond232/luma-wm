#!/usr/bin/env python3
"""Start an isolated nested session and verify policy and live shell reloads."""
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent


def request(path, command):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(3)
        stream.connect(str(path))
        stream.sendall((json.dumps({"version": 1, "command": command}) + "\n").encode())
        return json.loads(stream.makefile().readline())


def wait_for(path, predicate, process):
    deadline = time.monotonic() + 10
    last = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"compositor exited: {process.returncode}")
        try:
            last = request(path, "status")["state"]
            if predicate(last):
                return last
        except (OSError, ValueError):
            pass
        time.sleep(0.05)
    raise AssertionError(f"Timed out; last snapshot: {last}")


def fit_private_host(path, process, wid, max_width=1280, max_height=800):
    """Keep a nested output inside a host-constrained private Xwayland root."""
    if not os.environ.get("WM_CAPTURE_PRIVATE_X11"):
        return None
    display_width, display_height = map(
        int,
        subprocess.check_output(
            ["xdotool", "getdisplaygeometry"], text=True
        ).split(),
    )
    width = min(max_width, display_width)
    height = min(max_height, display_height)

    def host_geometry():
        values = {}
        for line in subprocess.check_output(
            ["xdotool", "getwindowgeometry", "--shell", wid], text=True
        ).splitlines():
            if "=" in line:
                key, value = line.split("=", 1)
                values[key] = value
        return int(values["WIDTH"]), int(values["HEIGHT"])

    if host_geometry() != (width, height):
        subprocess.run(["xdotool", "windowfocus", "--sync", wid], check=True, timeout=5)
        subprocess.run(
            ["xdotool", "windowsize", "--sync", wid, str(width), str(height)],
            check=True,
            timeout=5,
        )
    deadline = time.monotonic() + 5
    previous = None
    stable = 0
    while time.monotonic() < deadline:
        actual = host_geometry()
        stable = stable + 1 if actual == previous else 1
        previous = actual
        if stable >= 4:
            width, height = actual
            break
        time.sleep(0.05)
    else:
        raise AssertionError("private host window geometry did not settle")
    wait_for(
        path,
        lambda state: (
            state["outputs"][0]["geometry"]["w"],
            state["outputs"][0]["geometry"]["h"],
        )
        == (width, height),
        process,
    )
    return width, height


def main():
    # A private directory isolates IPC and config from every existing session.
    with tempfile.TemporaryDirectory(prefix="wm-check-") as directory:
        directory = Path(directory)
        config = directory / "config.toml"
        path = directory / "wm-nested.sock"
        original = (ROOT / "config/smoke.toml").read_text()
        config.write_text(original)
        title = "wm-test-" + directory.name
        env = dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(path), WM_NESTED_TITLE=title)
        with (directory / "session.log").open("w+") as log:
            process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], env=env,
                                       cwd=ROOT, stdout=log, stderr=log)
            try:
                wait_for(path, lambda s: any(l["namespace"] == "wm-bar" for l in s["layers"]), process)
                log.flush()
                startup_log = re.sub(
                    r"\x1b\[[0-9;]*m",
                    "",
                    (directory / "session.log").read_text(errors="replace"),
                )
                assert "Reserved isolated XWayland display" in startup_log, startup_log
                assert "xdisplay=100" in startup_log, startup_log
                assert "Failed to create sockets" not in startup_log, startup_log
                print("PASS: compositor XWayland uses isolated display :100")
                subprocess.run(["python3", str(ROOT / "tools/check-session.py")],
                               env=env, cwd=ROOT, check=True)

                def bar_at(bottom):
                    def matches(state):
                        bars = [l for l in state["layers"] if l["namespace"] == "wm-bar"]
                        return len(bars) == 1 and (bars[0]["geometry"]["y"] > 0) == bottom
                    return matches

                for _ in range(3):
                    config.write_text(original.replace(
                        "enabled = true", 'enabled = true\nposition = "bottom"\nheight = 64\nmodules = ["clock", "workspaces"]'))
                    wait_for(path, bar_at(True), process)
                    config.write_text(original)
                    wait_for(path, bar_at(False), process)
                config.write_text('invalid_setting = true\n')
                assert not request(path, "reload")["ok"]
                assert bar_at(False)(request(path, "status")["state"])
                config.write_text(original)
                assert request(path, "reload")["ok"]
                config.write_text(original + '\n[[rules]]\napp_id = "wm-rule-test"\nfloating = true\nwidth = 320\nheight = 200\nopacity = 0.5\n')
                assert request(path, "reload")["ok"]
                argv = ["kitty", "--class", "wm-rule-test", "-o", "confirm_os_window_close=0"]
                assert request(path, "exec " + json.dumps(argv))["ok"]
                state = wait_for(path, lambda s: any(w["app_id"] == "wm-rule-test" for w in s["windows"]), process)
                window = next(w for w in state["windows"] if w["app_id"] == "wm-rule-test")
                assert window["floating"] and window["geometry"]["w"] == 320 and window["geometry"]["h"] == 200, window
                assert window["opacity"] == 0.5, window
                assert request(path, f'focus {window["id"]}')["ok"]
                assert request(path, "fullscreen")["ok"]
                assert next(w for w in request(path, "status")["state"]["windows"] if w["id"] == window["id"])["opacity"] == 1.0
                assert request(path, "fullscreen")["ok"]
                assert next(w for w in request(path, "status")["state"]["windows"] if w["id"] == window["id"])["opacity"] == 0.5
                print("PASS: opacity rules and fullscreen opacity restoration")
                if os.environ.get("WM_CHECK_INTERACTION") == "1":
                    wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
                    if os.environ.get("WM_CAPTURE_PRIVATE_X11"):
                        fit_private_host(path, process, wid)
                        subprocess.run(["xdotool", "key", "Super_L+2"], check=True)
                        wait_for(
                            path,
                            lambda state: state["outputs"][0]["workspace"] == 2,
                            process,
                        )
                        subprocess.run(["xdotool", "key", "Super_L+1"], check=True)
                        wait_for(
                            path,
                            lambda state: state["outputs"][0]["workspace"] == 1,
                            process,
                        )
                        print("PASS: configured Super workspace keybinds dispatch")
                    else:
                        subprocess.run(["xdotool", "windowactivate", "--sync", wid], check=True)
                    # Host window animations can resize the nested output on activation.
                    time.sleep(0.5)
                    config.write_text(original + '\n[input]\nmouse_modifier = "Control"\n[[rules]]\napp_id = "org.customwm.InteractionTest"\nfloating = true\nwidth = 320\nheight = 200\n')
                    assert request(path, "reload")["ok"]
                    argv = ["python3", str(ROOT / "tools/interaction-client.py")]
                    assert request(path, "exec " + json.dumps(argv))["ok"]
                    def test_window(state):
                        return next((w for w in state["windows"] if w["app_id"] == "org.customwm.InteractionTest"), None)
                    state = wait_for(path, lambda s: test_window(s) is not None and test_window(s)["title"] == "Interaction test 320x200", process)
                    target = test_window(state)
                    def drag(x, y, dx, dy, button="1", modifier=None):
                        subprocess.run(["xdotool", "mousemove", "--sync", "--window", wid, str(x), str(y),
                                        "sleep", "0.2"] + (["keydown", modifier] if modifier else []) + ["mousedown", button, "sleep", "0.2",
                                        "mousemove", "--sync", "--window", wid, str(x + dx), str(y + dy),
                                        "sleep", "0.3", "mouseup", button] + (["keyup", modifier] if modifier else []), check=True)
                    rect = target["geometry"]
                    drag(rect["x"] + 100, rect["y"] + 60, 35, 25)
                    state = wait_for(path, lambda s: test_window(s)["geometry"]["x"] == rect["x"] + 35, process)
                    moved = test_window(state)["geometry"]
                    assert moved["y"] == rect["y"] + 25, moved
                    drag(moved["x"] + moved["w"] - 20, moved["y"] + moved["h"] - 20, 40, 30)
                    state = wait_for(path, lambda s: test_window(s)["geometry"]["w"] == moved["w"] + 40, process)
                    resized = test_window(state)["geometry"]
                    assert resized["h"] == moved["h"] + 30, resized
                    time.sleep(0.3)
                    assert test_window(request(path, "status")["state"])["geometry"] == resized
                    print("PASS: real Wayland pointer move and resize retain geometry")
                    drag(resized["x"] + resized["w"] - 60, resized["y"] + resized["h"] - 60,
                         25, 20, button="3", modifier="Control_L")
                    state = wait_for(path, lambda s: test_window(s)["geometry"]["w"] == resized["w"] + 25, process)
                    modified = test_window(state)["geometry"]
                    assert modified["h"] == resized["h"] + 20, modified
                    config.write_text(config.read_text().replace('mouse_modifier = "Control"', 'mouse_modifier = "disabled"'))
                    assert request(path, "reload")["ok"]
                    drag(modified["x"] + modified["w"] - 60, modified["y"] + modified["h"] - 60,
                         25, 20, button="3", modifier="Control_L")
                    assert test_window(request(path, "status")["state"])["geometry"] == modified
                    print("PASS: compositor modifier resize and live disabling")
                    drag(modified["x"] + 100, modified["y"] + 60, 10, 10)
                    wait_for(path, lambda s: test_window(s)["geometry"]["x"] == modified["x"] + 10, process)
                    print("PASS: floating window remains above tiled content after bar reload")
                    corner = test_window(request(path, "status")["state"])["geometry"]
                    subprocess.run(["xdotool", "mousemove", "--window", wid, str(corner["x"] + 1), str(corner["y"] + 1), "sleep", "0.15", "click", "1"], check=True)
                    assert request(path, "status")["state"]["focused"] != target["id"]
                    assert request(path, f'focus {target["id"]}')["ok"]
                    subprocess.run(["xdotool", "mousemove", "--window", wid, str(corner["x"] + 15), str(corner["y"] + 15), "sleep", "0.15", "click", "1"], check=True)
                    assert request(path, "status")["state"]["focused"] == target["id"]
                    print("PASS: clipped corners pass clicks through; visible interior receives clicks")
                    if os.environ.get("WM_CHECK_OPACITY") == "1":
                        from PIL import Image
                        sample = (corner["x"] + corner["w"] - 30, corner["y"] + 30)
                        before = directory / "opacity-before.png"
                        after = directory / "opacity-after.png"
                        subprocess.run(["import", "-window", wid, str(before)], check=True)
                        config.write_text(config.read_text() + '\n[[rules]]\napp_id="org.customwm.InteractionTest"\nopacity=0.5\n')
                        assert request(path, "reload")["ok"]
                        wait_for(path, lambda s: test_window(s)["opacity"] == 0.5, process)
                        a = Image.open(before).convert("RGB").getpixel(sample)
                        deadline = time.monotonic() + 3
                        while True:
                            subprocess.run(["import", "-window", wid, str(after)], check=True)
                            b = Image.open(after).convert("RGB").getpixel(sample)
                            if max(a) > 8 and all(abs(y - x * .5) <= 3 for x, y in zip(a, b)):
                                break
                            assert time.monotonic() < deadline, (a, b)
                            time.sleep(.03)
                        print("PASS: rendered pixel opacity changes on reload")
                    for _ in range(8):
                        assert request(path, "launcher")["ok"]
                    def launcher_ready(state):
                        layers = [layer for layer in state["layers"] if layer["namespace"] == "wm-launcher"]
                        if len(layers) != 1 or not layers[0].get("surface_size"):
                            return False
                        size = layers[0]["surface_size"]
                        return size[0] >= 600 and size[1] >= 450
                    wait_for(path, launcher_ready, process)
                    time.sleep(0.3)
                    assert len([l for l in request(path, "status")["state"]["layers"] if l["namespace"] == "wm-launcher"]) == 1
                    if screenshot := os.environ.get("WM_CHECK_LAUNCHER_SCREENSHOT"):
                        fit_private_host(path, process, wid)
                        wait_for(path, launcher_ready, process)
                        time.sleep(.15)
                        subprocess.run(["import", "-window", wid, screenshot], check=True)
                        if os.environ.get("WM_CHECK_LAUNCHER_ONLY") == "1":
                            print("PASS: rendered launcher after live output resize")
                            return
                    subprocess.run(["xdotool", "key", "Escape"], check=True)
                    wait_for(path, lambda s: not any(l["namespace"] == "wm-launcher" for l in s["layers"]), process)
                    assert request(path, "launcher")["ok"]
                    wait_for(path, lambda s: any(l["namespace"] == "wm-launcher" for l in s["layers"]), process)
                    subprocess.run(["xdotool", "key", "Escape"], check=True)
                    wait_for(path, lambda s: not any(l["namespace"] == "wm-launcher" for l in s["layers"]), process)
                    print("PASS: launcher singleton, dismissal and reopening")
                    config.write_text(original.replace('enabled = true', 'enabled = true\nmodules = ["notifications"]'))
                    assert request(path, "reload")["ok"]
                    time.sleep(0.3)
                    receipt = directory / "notifications.json"
                    assert request(path, "exec " + json.dumps(["python3", str(ROOT / "tools/notification-client.py"), str(receipt)]))["ok"]
                    state = wait_for(path, lambda s: receipt.with_suffix(".burst").exists() and any(l["namespace"] == "wm-notifications" for l in s["layers"]), process)
                    if os.environ.get("WM_CHECK_POPUP_SCREENSHOT"):
                        time.sleep(.2)
                        subprocess.run(["import", "-window", wid, os.environ["WM_CHECK_POPUP_SCREENSHOT"]], check=True)
                    receipt.with_suffix(".continue").touch()
                    wait_for(path, lambda s: receipt.exists() and any(l["namespace"] == "wm-notifications" for l in s["layers"]), process)
                    time.sleep(0.4)
                    popups = [l for l in request(path, "status")["state"]["layers"] if l["namespace"] == "wm-notifications"]
                    assert len(popups) == 1, popups
                    wait_for(path, lambda s: not any(l["namespace"] == "wm-notifications" for l in s["layers"]), process)
                    print("PASS: notification burst, replacement ID, cancelled old timeout and expiry")
                    assert request(path, "exec " + json.dumps(["notify-send", "-t", "1000", "History test", "First notification"]))["ok"]
                    wait_for(path, lambda s: any(l["namespace"] == "wm-notifications" for l in s["layers"]), process)
                    wait_for(path, lambda s: not any(l["namespace"] == "wm-notifications" for l in s["layers"]), process)
                    width = request(path, "status")["state"]["outputs"][0]["geometry"]["w"]
                    subprocess.run(["xdotool", "mousemove", "--window", wid, str(width - 75), "25", "click", "1"], check=True)
                    state = wait_for(path, lambda s: any(l["namespace"] == "wm-notification-center" for l in s["layers"]), process)
                    center_rect = next(l["geometry"] for l in state["layers"] if l["namespace"] == "wm-notification-center")
                    subprocess.run(["xdotool", "mousemove", "--sync", "--window", wid, str(center_rect["x"] + 70), str(center_rect["y"] + 25), "sleep", "0.2", "click", "1", "sleep", "0.2"], check=True)
                    if os.environ.get("WM_CHECK_SCREENSHOT"):
                        subprocess.run(["import", "-window", wid, os.environ["WM_CHECK_SCREENSHOT"]], check=True)
                    subprocess.run(["xdotool", "key", "Escape"], check=True)
                    wait_for(path, lambda s: not any(l["namespace"] == "wm-notification-center" for l in s["layers"]), process)
                    assert request(path, "exec " + json.dumps(["notify-send", "-t", "0", "Quiet test", "Saved during Do Not Disturb"]))["ok"]
                    time.sleep(0.5)
                    assert not any(l["namespace"] == "wm-notifications" for l in request(path, "status")["state"]["layers"])
                    print("PASS: notification delivery, history opening and Do Not Disturb suppression")
                    if os.environ.get("WM_CHECK_SCREENSHOT"):
                        subprocess.run(["xdotool", "mousemove", "--window", wid, str(width - 75), "25", "click", "1"], check=True)
                        wait_for(path, lambda s: any(l["namespace"] == "wm-notification-center" for l in s["layers"]), process)
                        time.sleep(0.2)
                        subprocess.run(["import", "-window", wid, os.environ["WM_CHECK_SCREENSHOT"]], check=True)
                with socket.socket(socket.AF_UNIX) as stream:
                    stream.settimeout(5)
                    stream.connect(str(path))
                    stream.sendall(b'{"version":1,"command":"subscribe"}\n')
                    reader = stream.makefile()
                    assert json.loads(reader.readline())["ok"]
                    assert json.loads(reader.readline())["ok"]
                print("PASS: repeated bar reloads, invalid config retention, floating size rules, subscription liveness")
            except BaseException:
                log.flush()
                log.seek(0)
                print(log.read()[-12000:])
                raise
            finally:
                if process.poll() is None:
                    try:
                        request(path, "quit")
                        process.wait(timeout=5)
                    except (OSError, subprocess.TimeoutExpired):
                        process.terminate()
                        process.wait(timeout=5)


if __name__ == "__main__":
    main()
