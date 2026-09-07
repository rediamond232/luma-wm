#!/usr/bin/env python3
"""Exercise audio controls and subscriptions on a real nested layer-shell surface."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from importlib.util import spec_from_file_location, module_from_spec

ROOT = Path(__file__).resolve().parent.parent
spec = spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = module_from_spec(spec)
spec.loader.exec_module(nested)

output = subprocess.check_output(["cargo", "test", "-p", "wm-shell", "--no-run", "--locked", "--offline", "--message-format=json"], cwd=ROOT, text=True)
artifacts = [json.loads(line) for line in output.splitlines()]
binary = next(a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact" and a.get("executable") and a["profile"]["test"])
with tempfile.TemporaryDirectory(prefix="wm-audio-input-") as temporary:
    directory = Path(temporary)
    config = directory / "config.toml"
    config.write_text("[shell]\nenabled=false\n[theme]\nblur=false\nanimation_ms=0\n")
    socket = directory / "wm.sock"
    receipt = directory / "ok"
    commands = directory / "bin"
    commands.mkdir()
    for name in ("wpctl", "pactl"):
        command = commands / name
        command.write_bytes((ROOT / "tools/mock-audio.py").read_bytes())
        command.chmod(0o755)
    title = directory.name
    log_path = directory / "session.log"
    with log_path.open("w") as log:
        process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT,
            env=dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket), WM_NESTED_TITLE=title), stdout=log, stderr=log)
        try:
            nested.wait_for(socket, lambda s: bool(s["outputs"]), process)
            wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
            subprocess.run(["xdotool", "windowactivate", "--sync", wid], check=True, timeout=5)
            command = ["env", "GDK_BACKEND=wayland", "WM_AUDIO_TEST_DIR=" + str(directory), "PATH=" + str(commands) + ":" + os.environ["PATH"],
                "WM_TEST_HOST_DISPLAY=" + os.environ["DISPLAY"], "WM_TEST_HOST_WINDOW=" + wid,
                binary,
                "audio::tests::live_controls_events_and_cleanup", "--ignored", "--test-threads=1", "--nocapture"]
            if os.environ.get("WM_CHECK_AUDIO_SCREENSHOT"):
                command.insert(1, "WM_AUDIO_TEST_SCREENSHOT=" + os.environ["WM_CHECK_AUDIO_SCREENSHOT"])
            assert nested.request(socket, "exec " + json.dumps(command))["ok"]
            deadline = time.monotonic() + 35
            while not receipt.exists():
                text = log_path.read_text(errors="replace")
                assert "test result: FAILED" not in text, text[-6000:]
                assert process.poll() is None and time.monotonic() < deadline, text[-6000:]
                time.sleep(.1)
            print("PASS: shared audio subscription/cached attach/multi-view updates and lifetime; keyboard output and microphone volume/mute/device selection, independent input failures/recovery, source event routing, nested popup focus, exact writes, rejected device selection, hotplug and reconnect lists, closed device-list read suppression, latest-target coalescing, denied/hung write recovery, event coalescing, idle read suppression, read failures/recovery, monitor restart/cleanup and popup focus")
        finally:
            Path(os.environ.get("WM_CHECK_AUDIO_LOG", "/tmp/luma-audio-input.log")).write_text(log_path.read_text(errors="replace"))
            if process.poll() is None:
                nested.request(socket, "quit")
                process.wait(timeout=5)
