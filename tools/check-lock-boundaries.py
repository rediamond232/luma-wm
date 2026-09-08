#!/usr/bin/env python3
"""Verify advertised globals and focus isolation on a dedicated nested compositor."""
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nested)
@contextlib.contextmanager
def desktop(config, socket, log, title):
    with log.open("w") as output:
        process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT,
            env=dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket), WM_NESTED_TITLE=title),
            stdout=output, stderr=output)
        try:
            nested.wait_for(socket, lambda state: bool(state["outputs"]), process)
            yield process
        finally:
            if process.poll() is None:
                nested.request(socket, "exit")
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=5)

with tempfile.TemporaryDirectory(prefix="luma-lock-rejection-") as temp:
    directory = Path(temp)
    binary = directory / "lock-rejection-client"
    flags = shlex.split(subprocess.check_output(["pkg-config", "--cflags", "--libs", "wayland-client"], text=True))
    subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-I", str(directory), str(ROOT / "tools/lock-boundary-client.c"), "-o", str(binary), *flags], check=True)
    config = directory / "config.toml"
    config.write_text('[shell]\nenabled=false\n[theme]\nanimation_ms=0\nblur=false\n')
    socket = directory / "wm.sock"
    log = directory / "session.log"
    try:
        with desktop(config, socket, log, "Luma lock rejection check") as compositor:
            result = nested.request(socket, "exec " + json.dumps(["env", "GDK_BACKEND=wayland", "WM_TEST_APP_ID=org.customwm.LockBaseline", "python3", str(ROOT / "tools/interaction-client.py")]))
            assert result["ok"], result
            baseline = nested.wait_for(
                socket,
                lambda state: len(state["windows"]) == 1
                and state["windows"][0]["surface_size"]
                == [
                    state["windows"][0]["geometry"]["w"],
                    state["windows"][0]["geometry"]["h"],
                ]
                and not state["windows"][0]["title"].endswith("0x0"),
                compositor,
            )
            receipt = directory / "receipt.json"
            assert nested.request(socket, "exec " + json.dumps(["env", "WM_LOCK_REJECTION_TEST=1", str(binary), str(receipt)]))["ok"]
            deadline = time.monotonic() + 8
            while not receipt.exists():
                assert compositor.poll() is None and time.monotonic() < deadline, log.read_text(errors="replace")[-5000:]
                time.sleep(.02)
            result = json.loads(receipt.read_text())
            assert result == {"lock_globals": 0, "input_globals": 0, "entered": 0}, result
            after = nested.request(socket, "status")["state"]
            assert after["focused"] == baseline["focused"] and after["windows"] == baseline["windows"], (baseline, after)
            assert nested.request(socket, "workspace 2")["ok"]
            assert nested.request(socket, "workspace 1")["ok"]
            print("PASS: nested backend withholds session-lock and virtual-input globals; unmapped client gains no keyboard focus; desktop remains usable")
    finally:
        Path("/tmp/luma-lock-rejection.log").write_text(log.read_text(errors="replace") if log.exists() else "no compositor log")
