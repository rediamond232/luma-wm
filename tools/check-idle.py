#!/usr/bin/env python3
"""Measure an isolated, empty nested compositor; excludes shell and client CPU."""
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


def descendants(pid):
    for child in Path(f"/proc/{pid}/task/{pid}/children").read_text().split():
        yield int(child)
        yield from descendants(int(child))


def counters(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    ticks = int(fields[11]) + int(fields[12])
    switches = sum(int(line.split()[1]) for line in Path(f"/proc/{pid}/status").read_text().splitlines()
                   if line.startswith(("voluntary_ctxt_switches:", "nonvoluntary_ctxt_switches:")))
    return ticks, switches


def main():
    with tempfile.TemporaryDirectory(prefix="wm-idle-") as directory:
        directory = Path(directory)
        config = directory / "config.toml"
        config.write_text("[shell]\nenabled=false\n[theme]\nblur=false\n")
        path = directory / "wm-nested.sock"
        env = dict(os.environ, WM_CONFIG=str(config), WM_SOCKET=str(path), WM_NESTED_TITLE=directory.name)
        with (directory / "session.log").open("w") as log:
            process = subprocess.Popen([str(ROOT / "tools/run-nested.sh")], cwd=ROOT, env=env, stdout=log, stderr=log)
            try:
                nested.wait_for(path, lambda s: bool(s["outputs"]), process)
                pid = next(pid for pid in descendants(process.pid)
                           if Path(f"/proc/{pid}/exe").resolve() == ROOT / "target/debug/wm")
                time.sleep(2)
                before = counters(pid)
                started = time.monotonic()
                time.sleep(5)
                elapsed = time.monotonic() - started
                after = counters(pid)
                cpu = (after[0] - before[0]) / os.sysconf("SC_CLK_TCK") / elapsed * 100
                print(f"Empty nested compositor: {cpu:.2f}% of one CPU, {(after[1]-before[1])/elapsed:.1f} main-thread context switches/s over {elapsed:.1f}s")
                assert nested.request(path, "status")["ok"]
            finally:
                if process.poll() is None:
                    nested.request(path, "quit")
                    process.wait(timeout=5)


if __name__ == "__main__":
    main()
