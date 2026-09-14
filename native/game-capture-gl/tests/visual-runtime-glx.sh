#!/usr/bin/env bash
# Controlled full-path GLX fixture: render a known pattern, record it, decode it.
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
repo_root=$(CDPATH= cd -- "$root/../.." && pwd)
muxer=${LUMA_GAME_CAPTURE_MUXER:-"$repo_root/target/release/luma-game-capture-muxer"}
seconds=${LUMA_GAME_CAPTURE_RUNTIME_SECONDS:-3}
fps=${LUMA_GAME_CAPTURE_TEST_FPS:-120}

skip() { echo "SKIP visual GLX fixture: $*" >&2; exit 77; }
[[ ${DISPLAY:-} ]] || skip 'DISPLAY is not set'
command -v glxinfo >/dev/null || skip 'glxinfo is unavailable'
command -v ffmpeg >/dev/null || skip 'ffmpeg is unavailable'
[[ -x $muxer ]] || skip "release muxer is unavailable at $muxer"
[[ -x $root/tests/glx-pattern && -x $root/tests/check-pattern-ppm ]] || skip 'fixture helpers are not built'
[[ $seconds =~ ^[1-9][0-9]*$ && $fps =~ ^[1-9][0-9]*$ && $fps -le 480 ]] || { echo 'invalid duration or FPS' >&2; exit 64; }
renderer=$(glxinfo -B 2>/dev/null | sed -n 's/^OpenGL renderer string: //p' | head -n 1 || true)
[[ $renderer == *NVIDIA* ]] || skip "current GLX renderer is not NVIDIA (${renderer:-unavailable})"

workdir=$(mktemp -d "${TMPDIR:-/tmp}/luma-visual-glx.XXXXXX")
output="$workdir/pattern.mp4"; ppm="$workdir/pattern.ppm"; log="$workdir/launcher.log"
record_pid= game_pid= keep=0
cleanup() {
    stop_target || true
    [[ -n ${record_pid:-} ]] && kill -0 "$record_pid" 2>/dev/null && kill -TERM "$record_pid" 2>/dev/null || true
    [[ -n ${record_pid:-} ]] && wait "$record_pid" 2>/dev/null || true
    if ((keep)); then echo "Fixture artifacts retained at: $workdir" >&2; else rm -rf -- "$workdir"; fi
}
trap cleanup EXIT

stop_target() {
    [[ -n ${game_pid:-} ]] && kill -0 "$game_pid" 2>/dev/null || return 0
    kill -TERM "$game_pid" 2>/dev/null || return 0
    # A broken GPU hook can strand the target inside a present call. The
    # fixture still addresses that exact PID (never a process group), then
    # bounds its cleanup rather than hanging the test runner indefinitely.
    for _ in $(seq 1 200); do
        kill -0 "$game_pid" 2>/dev/null || return 0
        sleep 0.01
    done
    kill -KILL "$game_pid" 2>/dev/null || true
}

echo "Visual GLX fixture: renderer=$renderer fps=$fps duration=${seconds}s" >&2
__GL_SYNC_TO_VBLANK=0 LUMA_GAME_CAPTURE_MUXER="$muxer" \
    "$root/luma-game-record-gl" --output "$output" --fps "$fps" -- \
    "$root/tests/glx-pattern" >"$log" 2>&1 &
record_pid=$!

# The recorder creates exactly one exec-preserved target child. Use that PID,
# rather than a wrapper/timeout, so the hook's expected-PID guard is exercised.
for _ in $(seq 1 500); do
    candidate=$(pgrep -P "$record_pid" glx-pattern || true)
    if [[ $candidate =~ ^[0-9]+$ ]]; then game_pid=$candidate; break; fi
    sleep 0.01
done
if [[ -z $game_pid ]]; then keep=1; echo 'FAIL visual GLX fixture: target PID not found' >&2; sed -n '1,160p' "$log" >&2 || true; exit 1; fi
sleep "$seconds"
stop_target
set +e; wait "$record_pid"; status=$?; set -e
record_pid=; game_pid=
# SIGKILL is only the bounded fallback for a target wedged inside present.
if [[ $status -ne 0 && $status -ne 137 && $status -ne 143 ]]; then keep=1; echo "FAIL visual GLX fixture: launcher exited $status" >&2; sed -n '1,160p' "$log" >&2 || true; exit 1; fi
if [[ ! -s $output ]]; then keep=1; echo 'FAIL visual GLX fixture: no MP4 was finalized' >&2; sed -n '1,160p' "$log" >&2 || true; exit 1; fi
ffmpeg -v error -ss 1 -i "$output" -frames:v 1 -f image2 -vcodec ppm -y "$ppm"
if ! "$root/tests/check-pattern-ppm" "$ppm"; then
    keep=1
    echo 'FAIL visual GLX fixture: decoded video does not preserve the expected pattern orientation' >&2
    exit 1
fi
echo "PASS visual GLX fixture: $output" >&2
