#!/usr/bin/env python3
"""Exercise Bluetooth pairing and controls on a private nested mock bus."""
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


def main():
    output = subprocess.check_output(["cargo", "test", "-p", "wm-shell", "--no-run", "--locked", "--offline", "--message-format=json"], cwd=ROOT, text=True)
    artifacts = [json.loads(line) for line in output.splitlines()]
    binary = next(a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact" and a.get("executable") and a["profile"]["test"])
    with tempfile.TemporaryDirectory(prefix="wm-bluetooth-pairing-") as temp:
        directory = Path(temp)
        config = directory / "config.toml"
        config.write_text("[shell]\nenabled=false\n[theme]\nblur=false\nanimation_ms=0\n")
        socket = directory / "wm.sock"
        receipt = directory / "ok"
        title = directory.name
        env = dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket), WM_NESTED_TITLE=title)
        log_path = directory / "session.log"
        with log_path.open("w") as log:
            process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT, env=env, stdout=log, stderr=log)
            try:
                nested.wait_for(socket, lambda s: bool(s["outputs"]), process)
                wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
                subprocess.run(["xdotool", "windowactivate", "--sync", wid], check=True, timeout=5)
                command = ["env", "GDK_BACKEND=wayland", "WM_NETWORK_TEST_PRIVATE_BUS=1", "WM_NETWORK_TEST_INPUT=1",
                           "WM_TEST_HOST_DISPLAY=" + os.environ["DISPLAY"], "WM_TEST_HOST_WINDOW=" + wid,
                           "WM_NETWORK_TEST_RECEIPT=" + str(receipt), binary,
                           ("bluetooth::tests::live_power_connections_and_restart" if os.environ.get("WM_CHECK_BLUETOOTH_CONTROLS") else "bluetooth_pairing::tests::live_pairing_prompts_and_cleanup"), "--ignored", "--test-threads=1"]
                if os.environ.get("WM_CHECK_NETWORK_SCREENSHOT"):
                    command.insert(1, "WM_NETWORK_TEST_SCREENSHOT=" + os.environ["WM_CHECK_NETWORK_SCREENSHOT"])
                result = nested.request(socket, "exec " + json.dumps(command))
                assert result["ok"], result
                deadline = time.monotonic() + 25
                while not receipt.exists():
                    text = log_path.read_text(errors="replace")
                    assert "test result: FAILED" not in text, text[-6000:]
                    assert process.poll() is None and time.monotonic() < deadline, text[-6000:]
                    time.sleep(.1)
                print("PASS: Bluetooth device-list pairing, Escape cancellation, device removal cancellation, power/discovery/connect lifecycle and restart" if os.environ.get("WM_CHECK_BLUETOOTH_CONTROLS") else "PASS: private BlueZ agent registration, keyboard PIN/passkey/confirmation, display progress, sender/device rejection, agent cancellation, user cancellation and connection cleanup")
            finally:
                Path(os.environ.get("WM_CHECK_BLUETOOTH_LOG", "/tmp/luma-bluetooth-pairing.log")).write_text(log_path.read_text(errors="replace"))
                if process.poll() is None:
                    nested.request(socket, "quit")
                    process.wait(timeout=5)


if __name__ == "__main__":
    main()
