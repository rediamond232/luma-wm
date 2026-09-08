#!/usr/bin/env python3
"""Exercise live output transforms and client layout in an isolated compositor."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
from importlib.util import spec_from_file_location, module_from_spec

ROOT = Path(__file__).resolve().parent.parent
spec = spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = module_from_spec(spec)
spec.loader.exec_module(nested)


def main():
    with tempfile.TemporaryDirectory(prefix="wm-transform-") as temporary:
        directory = Path(temporary)
        config = directory / "config.toml"
        original = (ROOT / "config/smoke.toml").read_text() + '\n'
        config.write_text(original)
        socket = directory / "wm.sock"
        title = directory.name
        env = dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(socket), WM_NESTED_TITLE=title)
        with (directory / "session.log").open("w") as log:
            process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT, env=env, stdout=log, stderr=log)
            try:
                state = nested.wait_for(socket, lambda s: bool(s["outputs"]) and bool(s["layers"]), process)
                wid = subprocess.check_output(
                    ["xdotool", "search", "--name", "^" + title + "$"], text=True
                ).strip().splitlines()[-1]
                nested.fit_private_host(socket, process, wid)
                state = nested.request(socket, "status")["state"]
                output = state["outputs"][0]
                name = output["name"]
                width, height = output["geometry"]["w"], output["geometry"]["h"]
                assert nested.request(socket, "terminal")["ok"]
                nested.wait_for(socket, lambda s: bool(s["windows"]), process)
                for transform in ("90", "180", "270", "flipped", "flipped-90", "flipped-180", "flipped-270", "normal"):
                    config.write_text(original + f'\n[outputs.{json.dumps(name)}]\ntransform={json.dumps(transform)}\n')
                    assert nested.request(socket, "reload")["ok"]
                    expected = (height, width) if transform in ("90", "270", "flipped-90", "flipped-270") else (width, height)
                def settled(state):
                    geometry = state["outputs"][0]["geometry"]
                    bars = [l for l in state["layers"] if l["namespace"] == "wm-bar"]
                    windows = state["windows"]
                    bar_width = bars[0]["geometry"]["w"] if bars else 0
                    bar_ready = bar_width > 0 and (
                        bar_width == expected[0]
                        or bool(os.environ.get("WM_CAPTURE_PRIVATE_X11"))
                    )
                    return (geometry["w"], geometry["h"]) == expected and bar_ready and windows and all(
                            w["geometry"]["x"] >= 0 and w["geometry"]["y"] >= 0 and
                            w["geometry"]["x"] + w["geometry"]["w"] <= expected[0] and
                            w["geometry"]["y"] + w["geometry"]["h"] <= expected[1] for w in windows)
                    nested.wait_for(socket, settled, process)
                config.write_text(original + f'\n[outputs.{json.dumps(name)}]\ntransform="90"\nscale=1.5\n')
                assert nested.request(socket, "reload")["ok"]
                nested.wait_for(socket, lambda s: abs(s["outputs"][0]["geometry"]["w"] * 1.5 - height) <= 1, process)
                config.write_text(original)
                assert nested.request(socket, "reload")["ok"]
                expected = (width, height)
                nested.wait_for(socket, settled, process)
                config.write_text(original + f'\n[outputs.{json.dumps(name)}]\ntransform="45"\n')
                assert not nested.request(socket, "reload")["ok"]
                print("PASS: live rotation/reflection, output/bar/client geometry, override removal and invalid-transform rejection")
            finally:
                if process.poll() is None:
                    nested.request(socket, "quit")
                    process.wait(timeout=5)


if __name__ == "__main__":
    main()
