#!/usr/bin/env python3
"""Exercise a dedicated nested test session; never use against a working desktop."""
import json
import os
import socket
import time

path = os.environ.get("WM_SOCKET", os.path.join(os.environ["XDG_RUNTIME_DIR"], "wm-nested.sock"))
if not path.endswith("wm-nested.sock"):
    raise SystemExit("This check only operates on the dedicated wm-nested.sock session")

def command(text):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(3)
        stream.connect(path)
        stream.sendall((json.dumps({"version": 1, "command": text}) + "\n").encode())
        result = json.loads(stream.makefile().readline())
    assert result["ok"], result
    return result["state"]

def wait(predicate):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        state = command("status")
        if predicate(state):
            return state
        time.sleep(0.05)
    raise AssertionError(state)

initial = command("status")
assert initial["outputs"] and all(o["name"] == "x11" for o in initial["outputs"])
assert all(w["app_id"] == "wm-smoke-terminal" for w in initial["windows"]), "Close unrelated test applications first"
while len(command("status")["windows"]) < 2:
    count = len(command("status")["windows"])
    command("terminal")
    wait(lambda s: len(s["windows"]) > count)
state = command("status")
first, second = [w["id"] for w in state["windows"][:2]]
command(f"focus {second}")
assert command("status")["focused"] == second
command("send 2")
assert next(w for w in command("status")["windows"] if w["id"] == second)["workspace"] == 2
command("workspace 2")
assert command("status")["focused"] == second
command("fullscreen")
state = command("status")
assert next(w for w in state["windows"] if w["id"] == second)["fullscreen"]
assert not state["outputs"][0]["wallpaper_visible"]
command("fullscreen")
command("floating")
floating = next(w for w in command("status")["windows"] if w["id"] == second)
assert floating["floating"]
saved_geometry = floating["geometry"]
command("fullscreen")
command("fullscreen")
assert next(w for w in command("status")["windows"] if w["id"] == second)["geometry"] == saved_geometry
command("floating")
command("floating")
assert next(w for w in command("status")["windows"] if w["id"] == second)["geometry"] == saved_geometry
command("scratchpad send")
assert next(w for w in command("status")["windows"] if w["id"] == second)["scratchpad"]
command("scratchpad show")
assert not next(w for w in command("status")["windows"] if w["id"] == second)["scratchpad"]
command("floating")
command("send 1")
command("workspace 1")
command("layout monocle")
command("layout master")
command("reload")
command(f"focus {second}")
command("close")
wait(lambda s: all(w["id"] != second for w in s["windows"]))
command(f"focus {first}")
print("PASS: control socket, focus, workspace transfer, fullscreen, wallpaper visibility, floating, scratchpad, layouts, reload, and close")
