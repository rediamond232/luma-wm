#!/usr/bin/env python3
"""Exercise real video playback, looping, pause/resume and error latching."""
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
    with tempfile.TemporaryDirectory(prefix="wm-video-") as directory:
        directory = Path(directory)
        video = directory / "test.webm"
        subprocess.run(["gst-launch-1.0", "-q", "videotestsrc", "pattern=ball", "num-buffers=60", "!",
                        "video/x-raw,width=320,height=180,framerate=30/1", "!", "vp8enc", "deadline=1", "!",
                        "webmmux", "!", "filesink", f"location={video}"], check=True)
        config = directory / "config.toml"
        original = (ROOT / "config/smoke.toml").read_text()
        config.write_text(original + f'\n[wallpaper]\nkind="video"\npath={json.dumps(str(video))}\nfps=15\n')
        path = directory / "wm-nested.sock"
        log_path = directory / "session.log"
        env = dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(path), WM_WALLPAPER_DEBUG="1")
        with log_path.open("w") as log:
            bus = subprocess.Popen(["dbus-daemon", "--session", "--nofork", "--print-address=1"],
                                   stdout=subprocess.PIPE, stderr=log, text=True)
            env["DBUS_SYSTEM_BUS_ADDRESS"] = bus.stdout.readline().strip()
            assert env["DBUS_SYSTEM_BUS_ADDRESS"], "private system-bus fixture did not start"
            (directory / "power-state").write_text("ac")
            power = subprocess.Popen(["/usr/bin/python3", str(ROOT / "tools/mock-power.py"), str(directory)],
                                     env=env, stdout=log, stderr=log)
            process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT, env=env, stdout=log, stderr=log)
            def wait_log(needle, offset=0):
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    content = log_path.read_text(errors="replace")
                    if any(item in content[offset:] for item in ([needle] if isinstance(needle, str) else needle)):
                        return len(content)
                    if process.poll() is not None:
                        raise RuntimeError("compositor exited")
                    time.sleep(.05)
                raise AssertionError(f"missing log event {needle}: {content[-4000:]}")
            def command(text):
                result = nested.request(path, text)
                assert result["ok"], result
                return result["state"]
            try:
                nested.wait_for(path, lambda s: bool(s["outputs"]), process)
                wait_log("wallpaper pipeline: Playing")
                wait_log("wallpaper loop: restarted")
                assert (directory / "power-ready").exists() and power.poll() is None
                offset = len(log_path.read_text())
                (directory / "power-state").write_text("battery")
                wait_log("wallpaper pipeline: Paused", offset)
                offset = len(log_path.read_text())
                config.write_text(config.read_text() + 'pause_on_battery=false\n')
                command("reload")
                wait_log("wallpaper pipeline: Playing", offset)
                offset = len(log_path.read_text())
                config.write_text(config.read_text().replace("pause_on_battery=false", "pause_on_battery=true"))
                command("reload")
                wait_log("wallpaper pipeline: Paused", offset)
                power.terminate()
                power.wait(timeout=5)
                (directory / "power-ready").unlink()
                (directory / "power-read").unlink()
                power = subprocess.Popen(["/usr/bin/python3", str(ROOT / "tools/mock-power.py"), str(directory)],
                                         env=env, stdout=log, stderr=log)
                deadline = time.monotonic() + 3
                while not (directory / "power-ready").exists() or not (directory / "power-read").exists():
                    assert power.poll() is None and time.monotonic() < deadline
                    time.sleep(.02)
                offset = len(log_path.read_text())
                (directory / "power-state").write_text("ac")
                wait_log("wallpaper pipeline: Playing", offset)
                print("PASS: UPower battery/AC events pause/resume decoded video; policy reload and service restart work")
                command("terminal")
                nested.wait_for(path, lambda s: bool(s["windows"]), process)
                offset = len(log_path.read_text())
                command("fullscreen")
                wait_log("wallpaper pipeline: Paused", offset)
                offset = len(log_path.read_text())
                command("fullscreen")
                wait_log("wallpaper pipeline: Playing", offset)
                print("PASS: decoded video playback, looping and fullscreen pause/resume")
                offset = len(log_path.read_text())
                config.write_text(config.read_text() + '\n[input]\nmouse_modifier="disabled"\n')
                wait_log("wallpaper loop: restarted", offset)
                assert "wallpaper pipeline: Ready" not in log_path.read_text()[offset:]
                print("PASS: unrelated config reload keeps the video pipeline alive")
                # With no gaps, corners or panel transparency, an ordinary
                # tiled client and the bar can cover the complete background.
                saved_config = config.read_text()
                covered_config = saved_config.replace("radius = 10.0", "radius = 0.0\ngap = 0\nborder = 0\nopacity = 1.0\nblur = false")
                offset = len(log_path.read_text())
                config.write_text(covered_config)
                command("reload")
                wait_log("wallpaper pipeline: Paused", offset)
                nested.wait_for(path, lambda s: not s["outputs"][0]["wallpaper_visible"], process)
                for exposed in [
                    covered_config + '\n[[rules]]\napp_id="wm-smoke-terminal"\nopacity=0.5\n',
                    covered_config.replace("radius = 0.0", "radius = 12.0"),
                ]:
                    offset = len(log_path.read_text())
                    config.write_text(exposed)
                    command("reload")
                    wait_log("wallpaper pipeline: Playing", offset)
                    nested.wait_for(path, lambda s: s["outputs"][0]["wallpaper_visible"], process)
                    offset = len(log_path.read_text())
                    config.write_text(covered_config)
                    command("reload")
                    wait_log("wallpaper pipeline: Paused", offset)
                offset = len(log_path.read_text())
                config.write_text(saved_config)
                command("reload")
                wait_log("wallpaper pipeline: Playing", offset)
                print("PASS: opaque coverage pauses video; opacity, corners and gaps resume it")
                corrupt = directory / "broken.webm"
                corrupt.write_bytes(b"not a media container")
                offset = len(log_path.read_text())
                config.write_text(original + f'\n[wallpaper]\nkind="video"\npath={json.dumps(str(corrupt))}\n')
                errors = ["wallpaper playback:", "wallpaper state transition failed:", "video wallpaper "]
                wait_log(errors, offset)
                time.sleep(.3)
                before = sum(log_path.read_text()[offset:].count(error) for error in errors)
                for _ in range(3):
                    command("fullscreen")
                    time.sleep(.15)
                    command("fullscreen")
                    time.sleep(.15)
                time.sleep(2.2)
                after = log_path.read_text()[offset:]
                assert sum(after.count(error) for error in errors) == before
                assert "wallpaper pipeline: Playing" not in after
                offset = len(log_path.read_text())
                config.write_text(original + f'\n[wallpaper]\nkind="video"\npath={json.dumps(str(video))}\n')
                wait_log("wallpaper pipeline: Playing", offset)
                print("PASS: corrupt media stays stopped through state updates; reload recovers")
            finally:
                try:
                    if process.poll() is None:
                        try:
                            command("quit")
                            process.wait(timeout=5)
                        finally:
                            if process.poll() is None:
                                process.terminate()
                                process.wait(timeout=5)
                finally:
                    try:
                        power.terminate()
                        power.wait(timeout=5)
                    finally:
                        bus.terminate()
                        bus.wait(timeout=5)


if __name__ == "__main__":
    main()
