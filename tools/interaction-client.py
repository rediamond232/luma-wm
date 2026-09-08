#!/usr/bin/env python3
"""Small Wayland client for testing real client-initiated compositor grabs."""
import os
import json
import time
from pathlib import Path
if os.environ.get("WM_TEST_PID_FILE"):
    Path(os.environ["WM_TEST_PID_FILE"]).write_text(str(os.getpid()))
os.environ["GDK_BACKEND"] = "wayland"
import gi
gi.require_version("Gtk", "4.0")
from gi.repository import Gtk, Gdk, GLib


def activate(app):
    if os.environ.get("WM_TEST_COLOR"):
        provider = Gtk.CssProvider()
        provider.load_from_string("window { background: " + os.environ["WM_TEST_COLOR"] + "; }")
        Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), provider, Gtk.STYLE_PROVIDER_PRIORITY_USER + 1)
    window = Gtk.ApplicationWindow(application=app, title="Interaction test")
    window.set_decorated(False)
    window.set_default_size(320, 200)
    label = Gtk.Label(label="Drag center to move\nDrag bottom-right corner to resize")
    window.set_child(label)
    if os.environ.get("WM_TEST_POPOVER"):
        provider = Gtk.CssProvider()
        provider.load_from_string("popover contents { background: #20c080; border: 0; border-radius: 0; box-shadow: none; padding: 0; }")
        Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), provider, Gtk.STYLE_PROVIDER_PRIORITY_USER + 2)
        popup = Gtk.Popover()
        popup.set_autohide(False)
        popup.set_has_arrow(False)
        popup.set_position(Gtk.PositionType.RIGHT)
        popup.set_parent(label)
        content = Gtk.Label(label="")
        content.set_size_request(100, 80)
        popup.set_child(content)
        def show_popup():
            point = Gdk.Rectangle()
            point.x = max(0, window.get_width() - 1)
            point.y = window.get_height() // 2
            point.width = point.height = 1
            popup.set_pointing_to(point)
            popup.popup()
            return False
        window.connect("map", lambda *_: GLib.idle_add(show_popup))
        window.connect("destroy", lambda *_: popup.unparent())

    if os.environ.get("WM_TEST_CURSOR"):
        texture = Gdk.Texture.new_from_filename(os.environ["WM_TEST_CURSOR"])
        window.set_cursor(Gdk.Cursor.new_from_texture(texture, 7, 9, None))
    elif os.environ.get("WM_TEST_CURSOR_NAME"):
        window.set_cursor_from_name(os.environ["WM_TEST_CURSOR_NAME"])
    if os.environ.get("WM_TEST_POINTER_RECEIPT"):
        receipt = Path(os.environ["WM_TEST_POINTER_RECEIPT"])
        motion = Gtk.EventControllerMotion()
        def pointer_event(_controller, x=None, y=None):
            value = {"x": x, "y": y, "width": window.get_width(),
                     "height": window.get_height(), "time": time.monotonic_ns()}
            temporary = receipt.with_suffix(".next")
            temporary.write_text(json.dumps(value))
            temporary.replace(receipt)
        motion.connect("enter", pointer_event)
        motion.connect("motion", pointer_event)
        motion.connect("leave", pointer_event)
        window.add_controller(motion)
    click = Gtk.GestureClick(button=1)

    def report_size(widget, clock):
        title = f"Interaction test {window.get_width()}x{window.get_height()}"
        if window.get_title() != title:
            window.set_title(title)
        return True

    label.add_tick_callback(report_size)

    def pressed(gesture, count, x, y):
        event = gesture.get_current_event()
        surface = window.get_surface()
        if x > window.get_width() - 40 and y > window.get_height() - 40:
            surface.begin_resize(Gdk.SurfaceEdge.SOUTH_EAST, event.get_device(), 1, x, y, event.get_time())
        else:
            surface.begin_move(event.get_device(), 1, x, y, event.get_time())

    click.connect("pressed", pressed)
    label.add_controller(click)
    window.present()


app = Gtk.Application(application_id=os.environ.get("WM_TEST_APP_ID", "org.customwm.InteractionTest"))
app.connect("activate", activate)
app.run(None)
