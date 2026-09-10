#!/usr/bin/env python3
"""StatusNotifierItem fixture with an exported com.canonical.dbusmenu."""
from pathlib import Path
import sys
import threading
from gi.repository import Gio, GLib

directory = Path(sys.argv[1])
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)

item_xml = """<node><interface name="org.kde.StatusNotifierItem">
<property name="Title" type="s" access="read"/>
<property name="Status" type="s" access="read"/>
<property name="IconName" type="s" access="read"/>
<property name="IconPixmap" type="a(iiay)" access="read"/>
<property name="OverlayIconPixmap" type="a(iiay)" access="read"/>
<property name="Menu" type="o" access="read"/>
<method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
<method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
</interface></node>"""
menu_xml = """<node><interface name="com.canonical.dbusmenu">
<method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
<method name="GetLayout"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/><arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/></method>
<method name="Event"><arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/></method>
</interface></node>"""


def item_property(_bus, _sender, _path, _interface, name):
    if name == "Title":
        return GLib.Variant("s", "Native menu fixture")
    if name == "Status":
        return GLib.Variant("s", "Active")
    if name == "IconName":
        return GLib.Variant("s", "")
    if name == "IconPixmap":
        return GLib.Variant("a(iiay)", [(18, 18, bytes([255, 40, 190, 90]) * 324)])
    if name == "OverlayIconPixmap":
        return GLib.Variant("a(iiay)", [])
    if name == "Menu":
        return GLib.Variant("o", "/Menu")
    raise AssertionError(name)


def item_call(_bus, _sender, _path, _interface, method, _args, invocation):
    (directory / "legacy-called").write_text(method)
    invocation.return_value(GLib.Variant("()", ()))


def menu_row(item_id, label, **properties):
    values = {"label": GLib.Variant("s", label)}
    values.update({key: GLib.Variant(kind, value) for key, (kind, value) in properties.items()})
    return GLib.Variant("(ia{sv}av)", (item_id, values, []))


def menu_call(_bus, _sender, _path, _interface, method, args, invocation):
    if method == "AboutToShow":
        invocation.return_value(GLib.Variant("(b)", (False,)))
    elif method == "GetLayout":
        parent, depth, _properties = args.unpack()
        assert depth == 1
        rows = [
            menu_row(1, "Open fixture"),
            menu_row(2, "Disabled", enabled=("b", False)),
            menu_row(3, "More", **{"children-display": ("s", "submenu")}),
        ] if parent == 0 else [menu_row(4, "Nested action")]
        layout = GLib.Variant("(ia{sv}av)", (parent, {}, rows))
        invocation.return_value(
            GLib.Variant.new_tuple(GLib.Variant("u", 1), layout)
        )
    elif method == "Event":
        item_id, event, _data, _timestamp = args.unpack()
        (directory / "menu-event").write_text(f"{item_id}:{event}")
        invocation.return_value(GLib.Variant("()", ()))


item_info = Gio.DBusNodeInfo.new_for_xml(item_xml)
menu_info = Gio.DBusNodeInfo.new_for_xml(menu_xml)
bus.register_object("/TestItem", item_info.interfaces[0], item_call, item_property, None)
bus.register_object("/Menu", menu_info.interfaces[0], menu_call, None, None)


def register():
    bus.call_sync(
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
        "RegisterStatusNotifierItem",
        GLib.Variant("(s)", ("/TestItem",)),
        None,
        Gio.DBusCallFlags.NONE,
        5000,
        None,
    )
    (directory / "ready").write_text("ok")


threading.Thread(target=register, daemon=True).start()
GLib.MainLoop().run()
