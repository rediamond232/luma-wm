#!/usr/bin/env python3
"""Private-bus UPower fixture; a test-owned file controls OnBattery."""
import os
from pathlib import Path
import sys
from gi.repository import Gio, GLib

directory = Path(sys.argv[1])
connection = Gio.DBusConnection.new_for_address_sync(
    os.environ["DBUS_SYSTEM_BUS_ADDRESS"],
    Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
    None, None,
)
interface = "org.freedesktop.UPower"
path = "/org/freedesktop/UPower"
state = (directory / "power-state").read_text().strip() == "battery"
info = Gio.DBusNodeInfo.new_for_xml("""<node><interface name="org.freedesktop.UPower">
<property name="OnBattery" type="b" access="read"/></interface></node>""")
def get_property(*_):
    (directory / "power-read").write_text("ok")
    return GLib.Variant("b", state)


connection.register_object(path, info.interfaces[0], None, get_property, None)
connection.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                     "RequestName", GLib.Variant("(su)", (interface, 4)), None,
                     Gio.DBusCallFlags.NONE, 1000, None)
(directory / "power-ready").write_text("ok")


def refresh():
    global state
    updated = (directory / "power-state").read_text().strip() == "battery"
    if updated != state:
        state = updated
        connection.emit_signal(None, path, "org.freedesktop.DBus.Properties", "PropertiesChanged",
                               GLib.Variant("(sa{sv}as)", (interface, {"OnBattery": GLib.Variant("b", state)}, [])))
    return True


GLib.timeout_add(20, refresh)
GLib.MainLoop().run()
