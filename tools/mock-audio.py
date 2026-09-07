#!/usr/bin/python3
"""Private fake wpctl/pactl executables for audio widget tests."""
import json
import os
from pathlib import Path
import sys
import time

directory = Path(os.environ["WM_AUDIO_TEST_DIR"])
if Path(sys.argv[0]).name == "pactl" and sys.argv[1:] == ["subscribe"]:
    with (directory / "monitor-starts").open("a") as output:
        output.write(str(os.getpid()) + "\n")
    (directory / "monitor-pid").write_text(str(os.getpid()))
    position = 0
    while not (directory / "disconnect").exists():
        path = directory / "events"
        text = path.read_text() if path.exists() else ""
        if len(text) > position:
            print(text[position:],end="",flush=True)
            position = len(text)
        time.sleep(.02)
    sys.exit(1)

if Path(sys.argv[0]).name == "pactl":
    with (directory / "device-calls").open("a") as output:
        output.write(json.dumps(sys.argv[1:])+"\n")
    source = any("source" in arg for arg in sys.argv[1:])
    sinks_path = directory / ("sources" if source else "sinks")
    sinks = json.loads(sinks_path.read_text()) if sinks_path.exists() else [{"name":"speakers","description":"Speakers"},{"name":"headphones","description":"Headphones"}]
    if sys.argv[1:] in (["get-default-sink"], ["get-default-source"]):
        path = directory / ("default-source" if source else "default-sink")
        print(path.read_text() if path.exists() else "speakers")
    elif sys.argv[1:] == ["-f","json","list", "sources" if source else "sinks"]:
        print(json.dumps(sinks))
    elif sys.argv[1] in ("set-default-sink", "set-default-source"):
        if (directory / "device-deny").exists():
            print("Simulated output selection failure",file=sys.stderr)
            sys.exit(1)
        assert sys.argv[2] in [sink["name"] for sink in sinks]
        (directory / ("default-source" if source else "default-sink")).write_text(sys.argv[2])
        with (directory / "events").open("a") as output:
            output.write("Event 'change' on server #0\n")
    else:
        raise AssertionError(sys.argv)
    sys.exit(0)

source = sys.argv[2] == "@DEFAULT_AUDIO_SOURCE@"
with (directory / ("input-calls" if source else "calls")).open("a") as output:
    output.write(json.dumps(sys.argv[1:]) + "\n")
state_path = directory / ("input-state" if source else "state")
state = json.loads(state_path.read_text())
if sys.argv[1] != "get-volume":
    (directory / "write-pid").write_text(str(os.getpid()))
    mode_path = directory / "write-mode"
    mode = mode_path.read_text() if mode_path.exists() else ""
    if mode == "deny":
        print("Simulated write failure",file=sys.stderr)
        sys.exit(1)
    if mode in ("hang", "slow"):
        time.sleep(10 if mode == "hang" else .3)
if state.get("fail"):
    print("Simulated audio failure",file=sys.stderr)
    sys.exit(1)
if sys.argv[1] == "get-volume":
    print("Volume: %.4f%s" % (state["volume"], " [MUTED]" if state["muted"] else ""))
elif sys.argv[1] == "set-volume":
    assert sys.argv[2] == ("@DEFAULT_AUDIO_SOURCE@" if source else "@DEFAULT_AUDIO_SINK@")
    state["volume"] = float(sys.argv[3])
elif sys.argv[1] == "set-mute":
    assert sys.argv[2:] == ["@DEFAULT_AUDIO_SOURCE@" if source else "@DEFAULT_AUDIO_SINK@","toggle"]
    state["muted"] = not state["muted"]
else:
    raise AssertionError(sys.argv)
if sys.argv[1] != "get-volume":
    temp = directory / ("state-" + str(os.getpid()))
    temp.write_text(json.dumps(state))
    temp.replace(state_path)
    with (directory / "events").open("a") as output:
        output.write("Event 'change' on %s #1\n" % ("source" if source else "sink"))
