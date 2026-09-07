#!/usr/bin/env python3
"""Exercise the actual layer-shell media popover on an isolated mock bus."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from importlib.util import spec_from_file_location, module_from_spec

ROOT = Path(__file__).resolve().parent.parent
spec = spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = module_from_spec(spec)
spec.loader.exec_module(nested)


def main():
    output = subprocess.check_output(["cargo", "test", "-p", "wm-shell", "--no-run", "--locked", "--offline", "--message-format=json"], cwd=ROOT, text=True)
    artifacts = [json.loads(line) for line in output.splitlines()]
    binary = next(a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact" and a.get("executable") and a["profile"]["test"])
    with tempfile.TemporaryDirectory(prefix="wm-media-input-") as temp:
        directory = Path(temp)
        config = directory / "config.toml"
        config.write_text("[shell]\nenabled=false\n[theme]\nblur=false\nanimation_ms=0\n")
        socket = directory / "wm.sock"
        receipt = directory / "ok"
        title = directory.name
        env = dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket), WM_NESTED_TITLE=title)
        log_path = directory / "session.log"
        server = None
        if os.environ.get("WM_CHECK_MEDIA_ART_RACES") == "1":
            class Artwork(BaseHTTPRequestHandler):
                def log_message(self, *args):
                    pass

                def do_GET(self):
                    phase = self.path.strip("/")
                    if phase not in ("old", "closed"):
                        self.send_error(404)
                        return
                    body = b"P6\n1 1\n255\n\x01\x02\x03"
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    (directory / (phase + ".started")).write_text("ok")
                    deadline = time.monotonic() + 8
                    while not (directory / (phase + ".release")).exists() and time.monotonic() < deadline:
                        time.sleep(.01)
                    try:
                        self.wfile.write(body)
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        pass
                    finally:
                        (directory / (phase + ".done")).write_text("ok")

            server = ThreadingHTTPServer(("127.0.0.1", 0), Artwork)
            threading.Thread(target=server.serve_forever, daemon=True).start()
        with log_path.open("w") as log:
            process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT, env=env, stdout=log, stderr=log)
            try:
                nested.wait_for(socket, lambda s: bool(s["outputs"]), process)
                wid = subprocess.check_output(["xdotool", "search", "--name", "^" + title + "$"], text=True).strip().splitlines()[-1]
                subprocess.run(["xdotool", "windowactivate", "--sync", wid], check=True, timeout=5)
                command = ["env", "GDK_BACKEND=wayland", "WM_NETWORK_TEST_PRIVATE_BUS=1", "WM_MEDIA_TEST_INPUT=1",
                           "WM_TEST_HOST_DISPLAY=" + os.environ["DISPLAY"], "WM_TEST_HOST_WINDOW=" + wid,
                           "WM_MEDIA_TEST_RECEIPT=" + str(receipt), binary,
                           "media::tests::live_player_discovery_controls_and_removal", "--ignored", "--test-threads=1", "--nocapture"]
                if os.environ.get("WM_CHECK_MEDIA_SCREENSHOT"):
                    command.insert(1, "WM_MEDIA_TEST_SCREENSHOT=" + os.environ["WM_CHECK_MEDIA_SCREENSHOT"])
                if server:
                    command.insert(1, "WM_MEDIA_ART_SERVER=http://127.0.0.1:" + str(server.server_port))
                    command.insert(1, "WM_MEDIA_ART_MARKERS=" + str(directory))
                result = nested.request(socket, "exec " + json.dumps(command))
                assert result["ok"], result
                deadline = time.monotonic() + 25
                while not receipt.exists():
                    text = log_path.read_text(errors="replace")
                    assert "test result: FAILED" not in text, text[-6000:]
                    assert process.poll() is None and time.monotonic() < deadline, text[-6000:]
                    time.sleep(.1)
                print("PASS: layer-shell media popup seeking, playback updates and keyboard-mode restoration")
            finally:
                if os.environ.get("WM_CHECK_MEDIA_LOG"):
                    Path(os.environ["WM_CHECK_MEDIA_LOG"]).write_text(log_path.read_text(errors="replace"))
                if server:
                    for phase in ("old", "closed"):
                        (directory / (phase + ".release")).write_text("ok")
                    server.shutdown()
                    server.server_close()
                if process.poll() is None:
                    nested.request(socket, "quit")
                    process.wait(timeout=5)


if __name__ == "__main__":
    main()
