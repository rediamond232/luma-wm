#!/usr/bin/env python3
"""Verify the native SCTK shell maps layer surfaces in a nested WM."""
import json
import os
from pathlib import Path
import re
import shlex
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


def wait_for(path, process, predicate):
    deadline = time.monotonic() + 10
    last = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"nested compositor exited: {process.returncode}")
        try:
            last = request(path, "status")["state"]
            if predicate(last):
                return last
        except (OSError, ValueError):
            pass
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for native shell: {last}")


def main():
    with tempfile.TemporaryDirectory(prefix="wm-sctk-") as directory:
        directory = Path(directory)
        config = directory / "config.toml"
        original = (ROOT / "config/sctk-smoke.toml").read_text()
        video = directory / "wallpaper.mp4"
        subprocess.run(
            [
                "ffmpeg", "-y", "-v", "error", "-f", "lavfi",
                "-i", "color=c=royalblue:s=16x16:r=2", "-t", "1",
                "-pix_fmt", "yuv420p", str(video),
            ],
            check=True,
        )
        next_video = directory / "wallpaper-next.mp4"
        subprocess.run(
            [
                "ffmpeg", "-y", "-v", "error", "-f", "lavfi",
                "-i", "color=c=darkorange:s=16x16:r=2", "-t", "1",
                "-pix_fmt", "yuv420p", str(next_video),
            ],
            check=True,
        )
        video_config = original + (
            "\n[wallpaper]\n"
            "kind = \"video\"\n"
            f"path = {json.dumps(str(video))}\n"
            "fps = 2\n"
        )
        if os.environ.get("WM_CHECK_VULKAN_PROFILE"):
            video_config += (
                "\n[recorder]\n"
                f"output_directory = {json.dumps(str(directory / 'captures'))}\n"
                "\n[[recorder.game_profiles]]\n"
                'name = "vulkan-ui-fixture"\n'
                'api = "vulkan"\n'
                'command = ["/usr/bin/vkcube", "--wsi", "xcb", "--present_mode", "0", "--c", "4000"]\n'
                "fps = 480\n"
            )
        config.write_text(video_config)
        socket_path = directory / "wm.sock"
        nested_title = "wm-sctk-shell-test"
        env = dict(
            os.environ,
            WM_CONFIG=str(config),
            WM_SOCKET=str(socket_path),
            WM_PRIVATE_BUS="1",
            WM_NESTED_TITLE=nested_title,
        )
        process = subprocess.Popen(
            [str(ROOT / "tools/run-nested.sh")], cwd=ROOT, env=env,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        try:
            state = wait_for(
                socket_path, process,
                lambda state: {
                    layer["namespace"]
                    for layer in state["layers"]
                    if layer.get("surface_size")
                } >= {"wm-bar", "wm-wallpaper"},
            )
            namespaces = {layer["namespace"] for layer in state["layers"]}
            assert "wm-shell" not in namespaces, namespaces
            assert all(layer.get("surface_size") for layer in state["layers"] if layer["namespace"] in {"wm-bar", "wm-wallpaper"}), state
            reloaded_config = video_config.replace(str(video), str(next_video)).replace(
                'backend = "sctk"',
                'backend = "sctk"\nposition = "bottom"\nheight = 64',
            )
            config.write_text(reloaded_config)
            assert request(socket_path, "reload")["ok"]
            wait_for(
                socket_path, process,
                lambda state: any(
                    layer["namespace"] == "wm-bar"
                    and layer["geometry"]["y"] > 0
                    and layer.get("surface_size", [0, 0])[1] == 64
                    for layer in state["layers"]
                ),
            )
            notification_info = directory / "notification-server.txt"
            argv = [
                "sh", "-lc",
                "gdbus call --session --dest org.freedesktop.Notifications "
                "--object-path /org/freedesktop/Notifications "
                "--method org.freedesktop.Notifications.GetServerInformation "
                f"> {notification_info}",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 7
            while not notification_info.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert "Luma SCTK shell" in notification_info.read_text(), notification_info.read_text()
            notification_capabilities = directory / "notification-capabilities.txt"
            argv = [
                "sh", "-lc",
                "gdbus call --session --dest org.freedesktop.Notifications "
                "--object-path /org/freedesktop/Notifications "
                "--method org.freedesktop.Notifications.GetCapabilities "
                f"> {notification_capabilities}",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 5
            while not notification_capabilities.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert "actions" in notification_capabilities.read_text(), notification_capabilities.read_text()
            tray_info = directory / "tray-watcher.txt"
            argv = [
                "sh", "-lc",
                "gdbus call --session --dest org.kde.StatusNotifierWatcher "
                "--object-path /StatusNotifierWatcher "
                "--method org.freedesktop.DBus.Introspectable.Introspect "
                f"> {tray_info}",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 5
            while not tray_info.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert "RegisterStatusNotifierItem" in tray_info.read_text(), tray_info.read_text()
            # A watcher removes items whose service connection has gone away.
            # Use the live, exported item fixture instead of registering a
            # made-up unowned well-known name. This fixture enters its GLib
            # loop before making the registration call, so it can answer the
            # watcher's synchronous property reads while registration runs.
            tray_item_ready = directory / "ready"
            tray_item_log = directory / "tray-item.log"
            argv = [
                "sh", "-lc",
                f"{shlex.join(['/usr/bin/python3', str(ROOT / 'tools/tray-menu-item.py'), str(directory)])} "
                f"> {shlex.quote(str(tray_item_log))} 2>&1",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 5
            while not tray_item_ready.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert tray_item_ready.exists(), (
                "StatusNotifierItem fixture did not register: "
                + (tray_item_log.read_text(errors="replace") if tray_item_log.exists() else "no output")
            )
            assert tray_item_ready.read_text().strip() == "ok"
            tray_items = directory / "tray-items.txt"
            argv = [
                "sh", "-lc",
                "gdbus call --session --dest org.kde.StatusNotifierWatcher "
                "--object-path /StatusNotifierWatcher "
                "--method org.freedesktop.DBus.Properties.Get "
                "org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems "
                f"> {tray_items}",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 5
            while (
                (not tray_items.exists() or "/TestItem" not in tray_items.read_text())
                and time.monotonic() < deadline
            ):
                time.sleep(0.05)
            assert "/TestItem" in tray_items.read_text(), tray_items.read_text()
            tray_host = directory / "tray-host.txt"
            argv = [
                "sh", "-lc",
                "gdbus call --session --dest org.kde.StatusNotifierWatcher "
                "--object-path /StatusNotifierWatcher "
                "--method org.freedesktop.DBus.Properties.Get "
                "org.kde.StatusNotifierWatcher IsStatusNotifierHostRegistered "
                f"> {tray_host}",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 5
            while not tray_host.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert "true" in tray_host.read_text(), tray_host.read_text()
            notification_reply = directory / "notification-reply.txt"
            argv = [
                "sh", "-lc",
                "gdbus call --session --dest org.freedesktop.Notifications "
                "--object-path /org/freedesktop/Notifications "
                "--method org.freedesktop.Notifications.Notify "
                "luma 0 '' 'SCTK notification' 'native overlay' [] '{}' -- -1 "
                f"> {notification_reply} 2>&1",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 5
            while not notification_reply.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert "uint32" in notification_reply.read_text(), notification_reply.read_text()
            wait_for(
                socket_path, process,
                lambda state: any(
                    layer["namespace"] == "wm-notifications"
                    and isinstance(layer.get("surface_size"), list)
                    and layer["surface_size"][0] > 100
                    for layer in state["layers"]
                ),
            )
            if screenshot := os.environ.get("WM_CHECK_POPUP_SCREENSHOT"):
                window = subprocess.check_output(
                    ["xdotool", "search", "--name", f"^{nested_title}$"], text=True
                ).strip().splitlines()[-1]
                subprocess.run(["import", "-window", window, screenshot], check=True)
            notification_close = directory / "notification-close.txt"
            argv = [
                "sh", "-lc",
                "gdbus call --session --dest org.freedesktop.Notifications "
                "--object-path /org/freedesktop/Notifications "
                "--method org.freedesktop.Notifications.CloseNotification 1 "
                f"> {notification_close} 2>&1",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 5
            while not notification_close.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert notification_close.read_text().strip() == "()", notification_close.read_text()
            notification_monitor = directory / "notification-monitor.txt"
            notification_monitor_ready = directory / "notification-monitor-ready.txt"
            argv = [
                "sh", "-lc",
                "timeout 8s stdbuf -oL gdbus monitor --session --dest org.freedesktop.Notifications "
                "--object-path /org/freedesktop/Notifications "
                f"> {notification_monitor} 2>&1 & echo ready > {notification_monitor_ready}",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            deadline = time.monotonic() + 5
            while not notification_monitor_ready.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert notification_monitor_ready.exists(), "notification signal monitor did not start"
            notification_expiry_reply = directory / "notification-expiry-reply.txt"
            argv = [
                "sh", "-lc",
                "gdbus call --session --dest org.freedesktop.Notifications "
                "--object-path /org/freedesktop/Notifications "
                "--method org.freedesktop.Notifications.Notify "
                "luma 0 '' 'SCTK expiry' 'five-second popup' [] '{}' 0 "
                f"> {notification_expiry_reply} 2>&1",
            ]
            assert request(socket_path, "exec " + json.dumps(argv))["ok"]
            expiry_started = time.monotonic()
            deadline = expiry_started + 7
            while not notification_expiry_reply.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            expiry_reply = notification_expiry_reply.read_text()
            expiry_id = re.search(r"uint32 (\d+)", expiry_reply)
            assert expiry_id, expiry_reply
            deadline = time.monotonic() + 7
            while time.monotonic() < deadline:
                monitor_text = notification_monitor.read_text() if notification_monitor.exists() else ""
                if re.search(
                    rf"NotificationClosed.*uint32 {expiry_id.group(1)}.*uint32 1",
                    monitor_text,
                    re.DOTALL,
                ):
                    break
                time.sleep(0.05)
            else:
                raise AssertionError(
                    "five-second notification did not emit NotificationClosed reason 1: "
                    + (notification_monitor.read_text() if notification_monitor.exists() else "")
                )
            assert time.monotonic() - expiry_started >= 4.5
            wait_for(
                socket_path, process,
                lambda state: any(
                    layer["namespace"] == "wm-notifications"
                    and layer.get("surface_size") == [1, 1]
                    for layer in state["layers"]
                ),
            )
            assert request(socket_path, "recorder")["ok"]
            wait_for(
                socket_path, process,
                lambda state: any(
                    layer["namespace"] == "wm-recorder"
                    and isinstance(layer.get("surface_size"), list)
                    and len(layer["surface_size"]) == 2
                    and layer["surface_size"][0] > 100
                    for layer in state["layers"]
                ),
            )
            window = None
            recorder_cycles = int(os.environ.get("WM_CHECK_RECORDER_CYCLES", "0"))
            if recorder_cycles:
                window = subprocess.check_output(
                    ["xdotool", "search", "--name", f"^{nested_title}$"], text=True
                ).strip().splitlines()[-1]
                subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                for _ in range(recorder_cycles):
                    subprocess.run(["xdotool", "key", "Right"], check=True)
                    time.sleep(0.1)
            time.sleep(1.5)
            if screenshot := os.environ.get("WM_CHECK_RECORDER_SCREENSHOT"):
                if window is None:
                    window = subprocess.check_output(
                        ["xdotool", "search", "--name", f"^{nested_title}$"], text=True
                    ).strip().splitlines()[-1]
                subprocess.run(["import", "-window", window, screenshot], check=True)
            if os.environ.get("WM_CHECK_RECORDER_START"):
                if window is None:
                    window = subprocess.check_output(
                        ["xdotool", "search", "--name", f"^{nested_title}$"], text=True
                    ).strip().splitlines()[-1]
                    subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                subprocess.run(["xdotool", "key", "Return"], check=True)
                capture_directory = directory / "captures"
                deadline = time.monotonic() + 20
                captures = []
                while time.monotonic() < deadline:
                    captures = list(capture_directory.glob("*.mp4"))
                    if captures and captures[0].stat().st_size > 0:
                        probe = subprocess.run(
                            [
                                "ffprobe", "-v", "error", "-select_streams", "v:0",
                                "-count_packets", "-show_entries", "stream=nb_read_packets",
                                "-of", "default=nw=1:nk=1", str(captures[0]),
                            ],
                            capture_output=True, text=True,
                        )
                        if probe.returncode == 0 and probe.stdout.strip().isdigit() and int(probe.stdout) > 0:
                            break
                    time.sleep(0.1)
                else:
                    raise AssertionError("native Vulkan recorder UI did not produce a video")
            launcher_requested = time.monotonic()
            assert request(socket_path, "launcher")["ok"]
            wait_for(socket_path, process, lambda state: any(layer["namespace"] == "wm-launcher" for layer in state["layers"]))
            assert time.monotonic() - launcher_requested < 2, "launcher surface took too long to appear"
            print("PASS: SCTK bar, video wallpaper, launcher, recorder, notification, and tray D-Bus services work without GTK")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()


if __name__ == "__main__":
    main()
