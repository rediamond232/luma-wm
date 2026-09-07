#!/usr/bin/env python3
"""Verify the tray registration service in an isolated nested shell."""
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

with tempfile.TemporaryDirectory(prefix="wm-tray-") as temporary:
    directory = Path(temporary)
    config = directory / "config.toml"
    config.write_text('[shell]\nmodules=["clock"]\n')
    socket = directory / "wm.sock"
    receipt = directory / "ok"
    log_path = directory / "session.log"
    with log_path.open("w") as log:
        process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT,
            env=dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket)), stdout=log, stderr=log)
        try:
            nested.wait_for(socket, lambda s: bool(s["layers"]), process)
            assert nested.request(socket, "exec " + json.dumps(["/usr/bin/python3", str(ROOT / "tools/tray-client.py"), str(receipt)]))["ok"]
            deadline = time.monotonic() + 12
            while not receipt.exists():
                assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(errors="replace")[-5000:]
                time.sleep(.05)
            print("PASS: tray registration, duplicate handling, caller ownership checks and item/host disconnect cleanup")
        finally:
            if process.poll() is None:
                nested.request(socket, "quit")
                process.wait(timeout=5)
