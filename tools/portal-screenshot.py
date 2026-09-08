#!/usr/bin/env python3
"""Request a non-interactive screenshot through xdg-desktop-portal."""

import json
from pathlib import Path
import sys
from urllib.parse import unquote, urlparse

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

receipt = Path(sys.argv[1])
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
loop = GLib.MainLoop()
request_path = None
result = {"error": "portal request timed out"}


def response(_connection, _sender, path, _interface, _signal, parameters):
    global result
    if request_path is not None and path != request_path:
        return
    code, values = parameters.unpack()
    if code != 0:
        result = {"error": f"portal response {code}"}
    else:
        uri = values.get("uri")
        parsed = urlparse(uri or "")
        if parsed.scheme != "file":
            result = {"error": f"unexpected screenshot URI {uri!r}"}
        else:
            path = Path(unquote(parsed.path))
            result = {"uri": uri, "path": str(path), "size": path.stat().st_size}
    loop.quit()


subscription = bus.signal_subscribe(
    "org.freedesktop.portal.Desktop",
    "org.freedesktop.portal.Request",
    "Response",
    None,
    None,
    Gio.DBusSignalFlags.NONE,
    response,
)
token = f"luma{GLib.get_monotonic_time()}"
reply = bus.call_sync(
    "org.freedesktop.portal.Desktop",
    "/org/freedesktop/portal/desktop",
    "org.freedesktop.portal.Screenshot",
    "Screenshot",
    GLib.Variant(
        "(sa{sv})",
        (
            "",
            {
                "handle_token": GLib.Variant("s", token),
                "interactive": GLib.Variant("b", False),
            },
        ),
    ),
    GLib.VariantType.new("(o)"),
    Gio.DBusCallFlags.NONE,
    10_000,
    None,
)
request_path = reply.unpack()[0]
GLib.timeout_add_seconds(15, lambda: loop.quit() or GLib.SOURCE_REMOVE)
loop.run()
bus.signal_unsubscribe(subscription)
receipt.write_text(json.dumps(result))
if "error" in result:
    raise SystemExit(result["error"])
