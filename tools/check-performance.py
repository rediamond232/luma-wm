#!/usr/bin/env python3
"""Measure a live Luma session without generating compositor work itself."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import time


ROOT = Path(__file__).resolve().parents[1]


def compositor_pid() -> int:
    result = subprocess.run(
        ["pgrep", "-xo", "wm"], text=True, capture_output=True, check=False
    )
    if result.returncode != 0 or not result.stdout.strip().isdigit():
        raise RuntimeError("no running Luma compositor named 'wm'")
    pid = int(result.stdout.strip())
    executable = Path(f"/proc/{pid}/exe").resolve()
    if executable.name != "wm":
        raise RuntimeError(f"PID {pid} is not a Luma compositor: {executable}")
    return pid


def cpu_ticks(pid: int) -> int:
    text = Path(f"/proc/{pid}/stat").read_text()
    fields = text[text.rfind(")") + 2 :].split()
    return int(fields[11]) + int(fields[12])


def memory(pid: int) -> dict[str, int]:
    wanted = {
        "Rss",
        "Pss",
        "Private_Clean",
        "Private_Dirty",
        "Anonymous",
        "Swap",
    }
    values: dict[str, int] = {}
    for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
        key, separator, rest = line.partition(":")
        if separator and key in wanted:
            values[f"{key.lower()}_kib"] = int(rest.split()[0])
    values["private_kib"] = values.get("private_clean_kib", 0) + values.get(
        "private_dirty_kib", 0
    )
    return values


def dmabuf_fds(pid: int) -> int:
    count = 0
    for entry in Path(f"/proc/{pid}/fd").iterdir():
        try:
            if "/dmabuf:" in os.readlink(entry):
                count += 1
        except OSError:
            pass
    return count


def wmctl() -> str:
    local = ROOT / "target" / "release" / "wmctl"
    if local.exists():
        return str(local)
    command = shutil.which("wmctl")
    if command:
        return command
    raise RuntimeError("wmctl was not found")


def performance_status() -> dict:
    result = subprocess.run(
        [wmctl(), "performance", "status"],
        text=True,
        capture_output=True,
        check=True,
    )
    return json.loads(result.stdout)["performance"]


def gpu_sample() -> dict[str, float] | None:
    if not shutil.which("nvidia-smi"):
        return None
    result = subprocess.run(
        [
            "nvidia-smi",
            "--query-gpu=utilization.gpu,memory.used,power.draw",
            "--format=csv,noheader,nounits",
        ],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0 or not result.stdout.strip():
        return None
    utilization, memory_mib, power_watts = result.stdout.splitlines()[0].split(",")
    return {
        "utilization_percent": float(utilization),
        "memory_mib": float(memory_mib),
        "power_watts": float(power_watts),
    }


def output_delta(before: dict, after: dict) -> list[dict]:
    old = {output["name"]: output for output in before.get("outputs", [])}
    counters = (
        "direct_scanout_frames",
        "composed_frames",
        "empty_frames",
        "missed_deadlines",
    )
    result = []
    for output in after.get("outputs", []):
        baseline = old.get(output["name"], {})
        item = dict(output)
        for counter in counters:
            item[f"{counter}_delta"] = output.get(counter, 0) - baseline.get(counter, 0)
        result.append(item)
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--duration", type=float, default=60.0)
    parser.add_argument("--interval", type=float, default=0.5)
    parser.add_argument("--label", default="manual")
    parser.add_argument(
        "--assert-idle",
        action="store_true",
        help="fail if CPU exceeds 1%% of one core or a presentation deadline is missed",
    )
    args = parser.parse_args()
    if args.duration <= 0 or args.interval <= 0:
        parser.error("duration and interval must be positive")

    pid = compositor_pid()
    before_status = performance_status()
    before_ticks = cpu_ticks(pid)
    started = time.monotonic()
    gpu = []
    while True:
        remaining = args.duration - (time.monotonic() - started)
        if remaining <= 0:
            break
        sample = gpu_sample()
        if sample:
            gpu.append(sample)
        time.sleep(min(args.interval, remaining))
    elapsed = time.monotonic() - started
    after_ticks = cpu_ticks(pid)
    after_status = performance_status()
    ticks_per_second = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
    cpu_percent_one_core = (after_ticks - before_ticks) / ticks_per_second / elapsed * 100.0

    report = {
        "label": args.label,
        "pid": pid,
        "duration_seconds": elapsed,
        "cpu_percent_one_core": cpu_percent_one_core,
        "memory": memory(pid),
        "dmabuf_fds": dmabuf_fds(pid),
        "configured_profile": after_status.get("configured_profile"),
        "active_profile": after_status.get("active_profile"),
        "outputs": output_delta(before_status, after_status),
    }
    if gpu:
        report["nvidia"] = {
            key: sum(sample[key] for sample in gpu) / len(gpu) for key in gpu[0]
        }
    print(json.dumps(report, indent=2, sort_keys=True))

    if args.assert_idle:
        missed = sum(
            output.get("missed_deadlines_delta", 0) for output in report["outputs"]
        )
        if cpu_percent_one_core > 1.0 or missed:
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
