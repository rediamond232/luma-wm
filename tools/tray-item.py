#!/usr/bin/env python3
"""Mock StatusNotifier item for real bar input and pixmap checks."""
from pathlib import Path
import sys
import json
from gi.repository import Gio, GLib

directory = Path(sys.argv[1])
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
interface = "org.kde.StatusNotifierItem"
status = "Active"
overlay = False
custom = ""
resolution = False
xml = '<node><interface name="' + interface + '">'
for name, kind in (("Title", "s"), ("Status", "s"), ("IconName", "s"), ("IconPixmap", "a(iiay)"), ("AttentionIconName", "s"), ("AttentionIconPixmap", "a(iiay)"), ("OverlayIconName", "s"), ("OverlayIconPixmap", "a(iiay)"), ("ItemIsMenu", "b")):
    xml += f'<property name="{name}" type="{kind}" access="read"/>'
xml += '<property name="ToolTip" type="(sa(iiay)ss)" access="read"/>'
xml += '<property name="IconThemePath" type="s" access="read"/>'
for method in ("Activate", "SecondaryActivate", "ContextMenu"):
    xml += f'<method name="{method}"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>'
xml += '<method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method>'
xml += '<signal name="NewIcon"/><signal name="NewOverlayIcon"/><signal name="NewStatus"><arg type="s"/></signal></interface></node>'
info = Gio.DBusNodeInfo.new_for_xml(xml)


def property_value(_bus, _sender, _path, _interface, name):
    if name == "ToolTip":
        return GLib.Variant("(sa(iiay)ss)", ("", [(32, 32, bytes([255, 220, 110, 40]) * (32 * 32))], "Tray details", "<b>Ready</b> &amp; waiting<br/><img src='/unused' alt='Status image'/>"))
    if name == "Title":
        return GLib.Variant("s", "Test tray icon")
    if name == "Status":
        return GLib.Variant("s", status)
    if name == "IconName":
        return GLib.Variant("s", "luma-fixture-icon" if custom else "")
    if name == "IconThemePath":
        path = "icons-a" if custom == "custom-relative" else str(directory / custom.removeprefix("custom-")) if custom else ""
        return GLib.Variant("s", path)
    if name == "IconPixmap":
        if resolution:
            return GLib.Variant("a(iiay)", [(18, 18, bytes([255, 32, 180, 80]) * 324),
                                             (36, 36, bytes([255, 130, 60, 210]) * 1296)])
        return GLib.Variant("a(iiay)", [(16, 16, bytes([255, 32, 180, 80]) * 256)])
    if name == "OverlayIconName":
        return GLib.Variant("s", "")
    if name == "AttentionIconName":
        return GLib.Variant("s", "")
    if name == "AttentionIconPixmap":
        return GLib.Variant("a(iiay)", [(16, 16, bytes([255, 40, 110, 220]) * 256)])
    if name == "OverlayIconPixmap":
        return GLib.Variant("a(iiay)", [(9, 9, bytes([255, 210, 40, 90]) * 81)] if overlay else [])
    return GLib.Variant("b", False)


def method_call(_bus, _sender, _path, _interface, method, args, invocation):
    if method == "Scroll":
        with (directory / "scrolls").open("a") as output:
            output.write(json.dumps(args.unpack()) + "\n")
    else:
        with (directory / "actions").open("a") as output:
            output.write(json.dumps([method, *args.unpack()]) + "\n")
    with (directory / "calls").open("a") as output:
        output.write(method + "\n")
    invocation.return_value(GLib.Variant("()", ()))


bus.register_object("/TestItem", info.interfaces[0], method_call, property_value, None)
bus.call_sync("org.kde.StatusNotifierWatcher", "/StatusNotifierWatcher", "org.kde.StatusNotifierWatcher",
              "RegisterStatusNotifierItem", GLib.Variant("(s)", ("/TestItem",)), None, Gio.DBusCallFlags.NONE, 2000, None)
(directory / "item-ready").write_text("ok")
loop = GLib.MainLoop()


def update():
    global status, overlay, custom, resolution
    command = (directory / "item-state").read_text().strip()
    if command == "quit":
        loop.quit()
        return False
    new_custom = command if command.startswith("custom-") else ""
    if resolution != (command == "resolution"):
        resolution = command == "resolution"
        bus.emit_signal(None, "/TestItem", interface, "NewIcon", GLib.Variant("()", ()))
    if custom != new_custom:
        custom = new_custom
        bus.emit_signal(None, "/TestItem", interface, "NewIcon", GLib.Variant("()", ()))
    new = "NeedsAttention" if command.startswith("attention") else "Passive" if command == "passive" else "Active"
    if overlay != (command in ("overlay", "attention-overlay")):
        overlay = command in ("overlay", "attention-overlay")
        bus.emit_signal(None, "/TestItem", interface, "NewOverlayIcon", GLib.Variant("()", ()))
    if new != status:
        status = new
        bus.emit_signal(None, "/TestItem", interface, "NewStatus", GLib.Variant("(s)", (status,)))
    return True


GLib.timeout_add(20, update)
loop.run()
