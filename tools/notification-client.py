#!/usr/bin/env python3
"""Send a burst and replace a short-lived notification over the test session bus."""
import json
from pathlib import Path
import sys
import time
import gi
from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)


def notify(summary, replace=0, timeout=0):
    reply = bus.call_sync("org.freedesktop.Notifications", "/org/freedesktop/Notifications",
                          "org.freedesktop.Notifications", "Notify",
                          GLib.Variant("(susssasa{sv}i)", ("luma-test", replace, "", summary,
                              "A notification body that wraps onto more than one line for layout coverage.", [], {}, timeout)),
                          GLib.VariantType.new("(u)"), Gio.DBusCallFlags.NONE, 3000, None)
    return reply.unpack()[0]


ids = [notify(f"Burst {i}") for i in range(12)]
receipt = Path(sys.argv[1])
receipt.with_suffix(".burst").touch()
deadline = time.monotonic() + 10
while not receipt.with_suffix(".continue").exists():
    if time.monotonic() > deadline:
        raise TimeoutError("test controller did not acknowledge burst")
    time.sleep(.05)
first = notify("Before replacement", timeout=100)
replacement = notify("Replacement must survive old timeout", replace=first, timeout=2500)
assert replacement == first
for item in ids:
    bus.call_sync("org.freedesktop.Notifications", "/org/freedesktop/Notifications",
                  "org.freedesktop.Notifications", "CloseNotification", GLib.Variant("(u)", (item,)),
                  None, Gio.DBusCallFlags.NONE, 3000, None)
receipt.write_text(json.dumps({"sent": len(ids), "replacement": replacement}))
