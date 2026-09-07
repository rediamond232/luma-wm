#!/usr/bin/env python3
"""Exercise the shell's watcher on its private session bus."""
import os
from pathlib import Path
import sys
import time
from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
name = "org.kde.StatusNotifierWatcher"
path = "/StatusNotifierWatcher"


def call(method, value, connection=bus):
    return connection.call_sync(name, path, name, method, GLib.Variant("(s)", (value,)), None,
                                Gio.DBusCallFlags.NONE, 2000, None)


def properties():
    return bus.call_sync(name, path, "org.freedesktop.DBus.Properties", "GetAll",
                         GLib.Variant("(s)", (name,)), None, Gio.DBusCallFlags.NONE, 2000, None).unpack()[0]


deadline = time.monotonic() + 5
while True:
    try:
        initial = properties()
        break
    except GLib.Error:
        assert time.monotonic() < deadline
        time.sleep(.05)
assert initial["RegisteredStatusNotifierItems"] == []
assert initial["ProtocolVersion"] == 0
assert not initial["IsStatusNotifierHostRegistered"]
call("RegisterStatusNotifierItem", "/TestItem")
call("RegisterStatusNotifierItem", "/TestItem")
item = bus.get_unique_name() + "/TestItem"
assert properties()["RegisteredStatusNotifierItems"] == [item]
for invalid in ("not a bus name", "/bad path", "org.freedesktop.DBus"):
    try:
        call("RegisterStatusNotifierItem", invalid)
        raise AssertionError("invalid or foreign registration accepted")
    except GLib.Error:
        pass
other = Gio.DBusConnection.new_for_address_sync(os.environ["DBUS_SESSION_BUS_ADDRESS"],
    Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION, None, None)
call("RegisterStatusNotifierItem", "/OtherItem", other)
call("RegisterStatusNotifierHost", other.get_unique_name(), other)
assert len(properties()["RegisteredStatusNotifierItems"]) == 2
assert properties()["IsStatusNotifierHostRegistered"]
other.close_sync(None)
deadline = time.monotonic() + 3
while True:
    current = properties()
    if current["RegisteredStatusNotifierItems"] == [item] and not current["IsStatusNotifierHostRegistered"]:
        break
    assert time.monotonic() < deadline, current
    time.sleep(.02)
for index in range(63):
    call("RegisterStatusNotifierItem", f"/Bounded{index}")
assert len(properties()["RegisteredStatusNotifierItems"]) == 64
try:
    call("RegisterStatusNotifierItem", "/Overflow")
    raise AssertionError("tray registry limit was not enforced")
except GLib.Error:
    pass
call("RegisterStatusNotifierItem", "/TestItem")
assert len(properties()["RegisteredStatusNotifierItems"]) == 64
Path(sys.argv[1]).write_text("ok")
