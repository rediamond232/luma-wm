#!/usr/bin/env python3
"""Run a separate VLC instance and observe its tray registration on the test bus."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from gi.repository import Gio, GLib

directory = Path(sys.argv[1])
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
watcher = "org.kde.StatusNotifierWatcher"
def call(service, path, interface, method, args):
    return bus.call_sync(service, path, interface, method, args, None, Gio.DBusCallFlags.NONE, 2000, None)
def items():
    return call(watcher, "/StatusNotifierWatcher", "org.freedesktop.DBus.Properties", "Get",
                GLib.Variant("(ss)", (watcher, "RegisteredStatusNotifierItems"))).unpack()[0]
def plain(value):
    if isinstance(value, GLib.Variant):
        return plain(value.unpack())
    if isinstance(value, (list, tuple)):
        return [plain(v) for v in value]
    if isinstance(value, dict):
        return {k: plain(v) for k, v in value.items()}
    return value

with (directory / "vlc.log").open("w") as log:
    player = subprocess.Popen(["vlc", "--intf=qt", "--no-one-instance", "--qt-system-tray",
        "--qt-start-minimized", "--no-qt-privacy-ask", "--no-media-library"],
        env=dict(os.environ, QT_QPA_PLATFORM="wayland", LC_ALL="C", XDG_CONFIG_HOME=str(directory / "vlc-config"),
                 XDG_CACHE_HOME=str(directory / "vlc-cache"), XDG_DATA_HOME=str(directory / "vlc-data")),
        stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 15
        while not items():
            assert player.poll() is None and time.monotonic() < deadline, (directory / "vlc.log").read_text()
            time.sleep(.05)
        name = items()[0]
        service, _, path = name.partition("/")
        path = "/" + path if path else "/StatusNotifierItem"
        properties = plain(call(service, path, "org.freedesktop.DBus.Properties", "GetAll",
                                GLib.Variant("(s)", ("org.kde.StatusNotifierItem",))))[0]
        assert "vlc" in (properties.get("Title", "") + service).lower(), properties
        menu_path = properties["Menu"]
        layout = plain(call(service, menu_path, "com.canonical.dbusmenu", "GetLayout",
                            GLib.Variant("(iias)", (0, 1, ["label", "enabled", "visible", "children-display"])) ))
        for row in layout[1][2]:
            row[1] = {key:value for key,value in row[1].items() if key in ("label", "enabled", "visible", "children-display", "type")}
        (directory / "vlc-ready.json").write_text(json.dumps({"service":service, "menu":menu_path, "layout":layout}))
        deadline = time.monotonic() + 30
        while player.poll() is None and not (directory / "stop").exists():
            assert time.monotonic() < deadline, "VLC tray action did not exit the application"
            time.sleep(.05)
        if player.poll() is not None:
            deadline = time.monotonic() + 3
            while items():
                assert time.monotonic() < deadline, "VLC tray registration was not removed"
                time.sleep(.05)
            (directory / "vlc-exited").write_text(str(player.returncode))
    finally:
        if player.poll() is None:
            player.terminate()
            player.wait(timeout=5)
