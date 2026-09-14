#!/usr/bin/env bash
# Exercise the complete opt-in OpenGL direct-capture path against glxgears.
#
# This is deliberately an integration fixture rather than part of `make test`:
# it needs the caller's live X11 session, an NVIDIA GL driver with NVENC, and
# a release luma-game-capture-muxer. It never attaches to another process.
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
repo_root=$(CDPATH= cd -- "$root/../.." && pwd)
muxer=${LUMA_GAME_CAPTURE_MUXER:-"$repo_root/target/release/luma-game-capture-muxer"}
seconds=${LUMA_GAME_CAPTURE_RUNTIME_SECONDS:-4}
fps=${LUMA_GAME_CAPTURE_TEST_FPS:-120}
min_duration=${LUMA_GAME_CAPTURE_MIN_DURATION_SECONDS:-}

skip() {
    echo "SKIP direct GLX runtime fixture: $*" >&2
    exit 77
}

[[ ${DISPLAY:-} ]] || skip 'DISPLAY is not set'
command -v glxinfo >/dev/null || skip 'glxinfo is unavailable'
command -v glxgears >/dev/null || skip 'glxgears is unavailable'
command -v ffprobe >/dev/null || skip 'ffprobe is unavailable'
command -v pgrep >/dev/null || skip 'pgrep is unavailable'
command -v ps >/dev/null || skip 'ps is unavailable'
[[ -x $muxer ]] || skip "release muxer is unavailable at $muxer"
[[ -r $root/build/libluma-game-capture-gl.so ]] || skip 'OpenGL hook is not built'
[[ $seconds =~ ^[1-9][0-9]*$ ]] || { echo 'runtime seconds must be a positive integer' >&2; exit 64; }
[[ $fps =~ ^[1-9][0-9]*$ && $fps -le 480 ]] || { echo 'test fps must be 1..480' >&2; exit 64; }
if [[ -z $min_duration ]]; then
    min_duration=$(awk -v seconds="$seconds" 'BEGIN { printf "%.3f", seconds / 2 }')
fi
[[ $min_duration =~ ^[0-9]+([.][0-9]+)?$ ]] || { echo 'minimum duration must be numeric' >&2; exit 64; }

renderer=$(glxinfo -B 2>/dev/null | sed -n 's/^OpenGL renderer string: //p' | head -n 1 || true)
[[ $renderer == *NVIDIA* ]] || skip "current GLX renderer is not NVIDIA (${renderer:-unavailable})"

workdir=$(mktemp -d "${TMPDIR:-/tmp}/luma-direct-glx.XXXXXX")
output="$workdir/direct-glx.mp4"
launcher_log="$workdir/launcher.log"
keep=0
record_pid=
game_pid=
cleanup() {
    # The test stops the actual GL target directly. Never use `timeout GAME`:
    # it becomes the configured root process and its forked child is correctly
    # excluded by the hook's target-PID guard.
    if [[ -n ${game_pid:-} ]] && kill -0 "$game_pid" 2>/dev/null; then
        kill -TERM "$game_pid" 2>/dev/null || true
    fi
    if [[ -n ${record_pid:-} ]] && kill -0 "$record_pid" 2>/dev/null; then
        kill -TERM "$record_pid" 2>/dev/null || true
        wait "$record_pid" 2>/dev/null || true
    fi
    if (( keep )); then
        echo "Fixture artifacts retained at: $workdir" >&2
    else
        rm -rf -- "$workdir"
    fi
}
trap cleanup EXIT

echo "Direct GLX fixture: renderer=$renderer fps=$fps duration=${seconds}s" >&2
__GL_SYNC_TO_VBLANK=0 LUMA_GAME_CAPTURE_MUXER="$muxer" \
    "$root/luma-game-record-gl" --output "$output" --fps "$fps" -- \
    glxgears -geometry 320x240 >"$launcher_log" 2>&1 &
record_pid=$!

# The recorder owns a muxer child as well as the exec-preserved GL target.
# Locate the latter before the requested test interval begins.
for _ in $(seq 1 500); do
    while read -r candidate; do
        [[ -n $candidate ]] || continue
        command=$(ps -o comm= -p "$candidate" 2>/dev/null | tr -d ' ')
        if [[ $command == glxgears ]]; then
            game_pid=$candidate
            break 2
        fi
    done < <(pgrep -P "$record_pid" || true)
    sleep 0.01
done
if [[ -z $game_pid ]]; then
    keep=1
    echo 'FAIL direct GLX fixture: did not find the direct glxgears target' >&2
    sed -n '1,200p' "$launcher_log" >&2 || true
    exit 1
fi

sleep "$seconds"
kill -TERM "$game_pid"
set +e
wait "$record_pid"
launch_status=$?
set -e
record_pid=
game_pid=

# glxgears commonly exits from TERM with 143; a normal zero exit is also fine.
if [[ $launch_status -ne 0 && $launch_status -ne 143 ]]; then
    keep=1
    echo "FAIL direct GLX fixture: launcher/glxgears exited $launch_status" >&2
    sed -n '1,200p' "$launcher_log" >&2 || true
    exit 1
fi

if [[ ! -s $output ]]; then
    keep=1
    echo 'FAIL direct GLX fixture: no MP4 was finalized' >&2
    sed -n '1,200p' "$launcher_log" >&2 || true
    exit 1
fi

probe=$(ffprobe -v error -select_streams v:0 -count_packets \
    -show_entries format=format_name,duration:stream=codec_type,codec_name,avg_frame_rate,nb_read_packets \
    -of default=noprint_wrappers=1 "$output")
[[ $probe == *'codec_type=video'* && $probe == *'codec_name=h264'* && $probe == *'format_name='*mp4* ]] || {
    keep=1
    echo 'FAIL direct GLX fixture: finalized file is not an H.264 MP4' >&2
    printf '%s\n' "$probe" >&2
    exit 1
}

duration=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$output")
if ! awk -v actual="$duration" -v minimum="$min_duration" 'BEGIN { exit !(actual >= minimum) }'; then
    keep=1
    echo "FAIL direct GLX fixture: MP4 duration ${duration}s is below ${min_duration}s" >&2
    printf '%s\n' "$probe" >&2
    sed -n '1,200p' "$launcher_log" >&2 || true
    exit 1
fi

packet_count=$(sed -n 's/^nb_read_packets=//p' <<<"$probe")
frame_rate=$(sed -n 's/^avg_frame_rate=//p' <<<"$probe")
if [[ ! $packet_count =~ ^[1-9][0-9]*$ || ! $frame_rate =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]]; then
    keep=1
    echo 'FAIL direct GLX fixture: ffprobe did not report usable packet/timing evidence' >&2
    printf '%s\n' "$probe" >&2
    exit 1
fi
actual_fps=$(awk -F/ -v rate="$frame_rate" 'BEGIN { split(rate, p, "/"); printf "%.3f", p[1] / p[2] }')
minimum_fps=$(awk -v requested="$fps" 'BEGIN { printf "%.3f", requested * 0.5 }')
if ! awk -v actual="$actual_fps" -v minimum="$minimum_fps" 'BEGIN { exit !(actual >= minimum) }'; then
    keep=1
    echo "FAIL direct GLX fixture: actual ${actual_fps} FPS is below ${minimum_fps} FPS" >&2
    printf '%s\n' "$probe" >&2
    exit 1
fi

echo "PASS direct GLX fixture: NVENC direct hook produced a decodable H.264 MP4 (${packet_count} real packets, ${actual_fps} FPS)" >&2
printf '%s\n' "$probe"
