#!/usr/bin/env python3
"""Negotiate a monitor stream through the ScreenCast portal."""

import json
from pathlib import Path
import subprocess
import sys
import time
import re

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

receipt = Path(sys.argv[1])
config = Path(sys.argv[2])
use_dmabuf = len(sys.argv) > 3 and sys.argv[3] == "--dmabuf"
root = Path(__file__).resolve().parent.parent
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
responses = {}
loop = GLib.MainLoop()


def response(_connection, _sender, path, _interface, _signal, parameters):
    responses[path] = parameters.unpack()
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
sequence = 0


def request(method, signature, arguments, options):
    global sequence
    sequence += 1
    token = f"luma{GLib.get_monotonic_time()}{sequence}"
    options = dict(options, handle_token=GLib.Variant("s", token))
    reply = bus.call_sync(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
        method,
        GLib.Variant(signature, (*arguments, options)),
        GLib.VariantType.new("(o)"),
        Gio.DBusCallFlags.NONE,
        10_000,
        None,
    )
    path = reply.unpack()[0]
    while path not in responses:
        timed_out = {"value": False}

        def timeout():
            timed_out["value"] = True
            loop.quit()
            return GLib.SOURCE_REMOVE

        source = GLib.timeout_add_seconds(15, timeout)
        loop.run()
        GLib.source_remove(source)
        if timed_out["value"] and path not in responses:
            raise RuntimeError(f"{method} portal request timed out")
    code, values = responses.pop(path)
    if code != 0:
        raise RuntimeError(f"{method} portal response {code}")
    return values


try:
    created = request(
        "CreateSession",
        "(a{sv})",
        (),
        {"session_handle_token": GLib.Variant("s", f"session{GLib.get_monotonic_time()}")},
    )
    session = created["session_handle"]
    request(
        "SelectSources",
        "(oa{sv})",
        (session,),
        {
            "types": GLib.Variant("u", 1),
            "multiple": GLib.Variant("b", False),
            # Keep this stream independent of physical host-pointer movement so
            # the idle interval measures scene damage rather than cursor damage.
            # Cursor composition is covered by the legacy screencopy fixture.
            "cursor_mode": GLib.Variant("u", 1),
        },
    )
    started = request("Start", "(osa{sv})", (session, ""), {})
    streams = started.get("streams", [])
    if len(streams) != 1:
        raise RuntimeError(f"expected one portal stream, got {streams!r}")
    node, properties = streams[0]
    remote, descriptors = bus.call_with_unix_fd_list_sync(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
        "OpenPipeWireRemote",
        GLib.Variant("(oa{sv})", (session, {})),
        GLib.VariantType.new("(h)"),
        Gio.DBusCallFlags.NONE,
        10_000,
        None,
        None,
    )
    remote_fd = descriptors.get(remote.unpack()[0])
    frame_pattern = receipt.with_name("stream-%d.png")
    conversion = ["!", "videoconvert"]
    if use_dmabuf:
        conversion = [
            "!",
            "video/x-raw(memory:DMABuf)",
            "!",
            "glupload",
            "!",
            "glcolorconvert",
            "!",
            "gldownload",
            "!",
            "video/x-raw,format=RGBA",
        ]
    pipeline = subprocess.Popen(
        [
            "gst-launch-1.0",
            "-q",
            "pipewiresrc",
            f"fd={remote_fd}",
            f"path={node}",
            "num-buffers=2",
            *conversion,
            "!",
            "pngenc",
            "!",
            "multifilesink",
            f"location={frame_pattern}",
        ],
        pass_fds=(remote_fd,),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    time.sleep(0.5)
    if pipeline.poll() is not None:
        stdout, stderr = pipeline.communicate()
        if pipeline.returncode != 0:
            raise RuntimeError(f"GStreamer exited with status {pipeline.returncode}: {stderr or stdout}")
        raise RuntimeError("copy_with_damage produced a second frame without compositor damage")
    config_text = config.read_text()
    if re.search(r"(?m)^background\s*=", config_text):
        config_text = re.sub(
            r"(?m)^background\s*=.*$", 'background="#263247"', config_text, count=1
        )
    else:
        config_text += 'background="#263247"\n'
    config.write_text(config_text)
    subprocess.run([str(root / "target/debug/wmctl"), "reload"], check=True)
    deadline = time.monotonic() + 15
    while pipeline.poll() is None and time.monotonic() < deadline:
        time.sleep(0.1)
    if pipeline.poll() is None:
        pipeline.kill()
        pipeline.wait()
        raise RuntimeError("PipeWire stream did not deliver a frame after compositor damage")
    if pipeline.returncode != 0:
        stdout, stderr = pipeline.communicate()
        raise RuntimeError(f"GStreamer exited with status {pipeline.returncode}: {stderr or stdout}")
    frames = sorted(receipt.parent.glob("stream-*.png"))
    if len(frames) != 2 or any(frame.stat().st_size == 0 for frame in frames):
        raise RuntimeError(f"PipeWire stream produced {len(frames)} complete video frames")
    receipt.write_text(
        json.dumps(
            {
                "session": session,
                "node": node,
                "frames": [str(frame) for frame in frames],
                "position": properties.get("position"),
                "size": properties.get("size"),
            }
        )
    )
    bus.call_sync(
        "org.freedesktop.portal.Desktop",
        session,
        "org.freedesktop.portal.Session",
        "Close",
        None,
        None,
        Gio.DBusCallFlags.NONE,
        5_000,
        None,
    )
finally:
    bus.signal_unsubscribe(subscription)
