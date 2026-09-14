#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
muxer=$root/../../target/release/luma-game-capture-muxer
for command in vkcube ffprobe ffmpeg pgrep; do
    command -v "$command" >/dev/null || {
        echo "SKIP: Vulkan integration needs $command" >&2
        exit 77
    }
done
[[ -n ${DISPLAY:-}${WAYLAND_DISPLAY:-} && -x $muxer ]] || {
    echo "SKIP: Vulkan integration needs a graphical session and release muxer" >&2
    exit 77
}
vulkaninfo --summary 2>/dev/null | grep -q 'driverName.*NVIDIA' || {
    echo "SKIP: Vulkan/NVENC integration currently requires an NVIDIA device" >&2
    exit 77
}

work=$(mktemp -d -- "${XDG_RUNTIME_DIR:-/tmp}/luma-vulkan-integration.XXXXXX")
output=$work/capture.mp4
log=$work/capture.log
test_fps=${LUMA_VULKAN_TEST_FPS:-480}
[[ $test_fps =~ ^[0-9]+$ && $test_fps -ge 30 && $test_fps -le 480 ]] || {
    echo "LUMA_VULKAN_TEST_FPS must be between 30 and 480" >&2
    exit 64
}
wrapper_pid= game_pid=
cleanup() {
    [[ -n ${wrapper_pid:-} ]] && kill -TERM "$wrapper_pid" 2>/dev/null || true
    [[ -n ${wrapper_pid:-} ]] && wait "$wrapper_pid" 2>/dev/null || true
    [[ -n ${game_pid:-} ]] && kill -TERM "$game_pid" 2>/dev/null || true
    if [[ -n ${LUMA_KEEP_VULKAN_TEST:-} ]]; then
        echo "Vulkan integration files: $work" >&2
    else
        rm -rf -- "$work"
    fi
}
trap cleanup EXIT

LUMA_GAME_CAPTURE_MUXER=$muxer "$root/luma-game-record-vulkan" \
    --output "$output" --fps "$test_fps" --quality 45 -- \
    vkcube --wsi xcb --present_mode 0 --c 10000000 >"$log" 2>&1 &
wrapper_pid=$!
for _ in $(seq 1 500); do
    game_pid=$(pgrep -P "$wrapper_pid" -x vkcube | head -n1 || true)
    [[ -n $game_pid ]] && break
    kill -0 "$wrapper_pid" 2>/dev/null || break
    sleep 0.01
done
[[ -n $game_pid ]] || { cat "$log" >&2; exit 1; }

sleep 2
kill -TERM "$wrapper_pid"
wait "$wrapper_pid"
wrapper_pid=
kill -0 "$game_pid"

packets=$(ffprobe -v error -select_streams v:0 -count_packets \
    -show_entries stream=nb_read_packets -of default=nw=1:nk=1 "$output")
minimum_packets=$((test_fps / 4))
[[ $packets =~ ^[0-9]+$ && $packets -ge $minimum_packets ]] || {
    cat "$log" >&2
    echo "expected at least $minimum_packets Vulkan packets, got $packets" >&2
    exit 1
}
ffmpeg -v error -xerror -threads 1 -i "$output" -map 0:v:0 \
    -c:v rawvideo -f rawvideo -y /dev/null
ffmpeg -v error -xerror -i "$output" -frames:v 30 -f framemd5 "$work/frames.md5"
frames=$(awk -F', ' '/^0,/ {count++} END {print count+0}' "$work/frames.md5")
unique=$(awk -F', ' '/^0,/ {print $6}' "$work/frames.md5" | sort -u | wc -l)
[[ $frames -ge 10 && $frames = "$unique" ]] || {
    echo "decoded Vulkan sample contains duplicate frames ($unique unique of $frames)" >&2
    exit 1
}

kill -TERM "$game_pid"
for _ in $(seq 1 300); do
    kill -0 "$game_pid" 2>/dev/null || break
    sleep 0.01
done
game_pid=
echo "PASS: Vulkan API capture wrote $packets packets, decoded cleanly with $frames/$unique distinct sampled frames, and left the game alive on recorder stop"
