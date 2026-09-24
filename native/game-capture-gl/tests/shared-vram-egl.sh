#!/usr/bin/env bash
# EGL desktop GL, 1440p pixels, external encoder ownership, and backpressure.
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d /tmp/luma-shared-vram.XXXXXX)
wrapper= game= receiver=
cleanup() {
    [[ -z $receiver ]] || kill -CONT "$receiver" 2>/dev/null || true
    [[ -z $wrapper ]] || kill -TERM "$wrapper" 2>/dev/null || true
    [[ -z $wrapper ]] || wait "$wrapper" 2>/dev/null || true
    [[ -z $game ]] || kill -TERM "$game" 2>/dev/null || true
    echo "EGL shared-VRAM test artifacts: $work"
}
trap cleanup EXIT
LUMA_GAME_CAPTURE_DEBUG=1 "$root/luma-game-record-gl" --output "$work/video.mp4" --fps 240 -- \
    "$root/tests/egl-pattern" >"$work/log" 2>&1 & wrapper=$!
for _ in $(seq 1 500); do
    game=$(pgrep -P "$wrapper" -x egl-pattern || true)
    receiver=$(pgrep -P "$wrapper" -f '/luma-opengl-capture-receiver ' || true)
    if [[ -n $game && -n $receiver ]] && grep -q 'shared VRAM pool ready' "$work/log"; then break; fi
    sleep 0.01
done
[[ -n $game && -n $receiver ]]
grep -q 'shared VRAM pool ready' "$work/log"
sleep 2
! grep -q 'libnvidia-encode' "/proc/$game/maps"
grep -q 'libnvidia-encode' "/proc/$receiver/maps"
kill -STOP "$receiver"
sleep 0.5
before=$(grep '^FRAME ' "$work/log" | tail -1)
sleep 1
after=$(grep '^FRAME ' "$work/log" | tail -1)
[[ $before != "$after" ]] || { echo 'Game blocked on stalled receiver'; exit 1; }
kill -CONT "$receiver"
sleep 2
kill -TERM "$wrapper"
wait "$wrapper"
wrapper= receiver=
# Stop must drain the worker's bounded staging queue and every submitted
# bitstream before the muxer sees EOF. This catches premature context teardown.
grep -q 'Luma encode worker: staging capacity=' "$work/log"
! grep -q 'direct NVENC disabled\|GPU encoder worker failed' "$work/log"
submitted=$(sed -n 's/.*Luma GPU receiver processed [0-9]* exports and submitted \([0-9]*\) frames/\1/p' "$work/log" | tail -1)
written=$(ffprobe -v error -select_streams v:0 -show_entries stream=nb_frames -of csv=p=0 "$work/video.mp4")
[[ $submitted =~ ^[1-9][0-9]*$ && $submitted == "$written" ]] || {
    echo "Worker drain mismatch: submitted=$submitted written=$written"; exit 1;
}
kill -0 "$game"
sleep 1.5
grep -q 'stopped and released direct capture resources' "$work/log"
ffmpeg -v error -ss 1 -i "$work/video.mp4" -frames:v 1 "$work/frame.ppm"
"$root/tests/check-pattern-ppm" "$work/frame.ppm"
ffmpeg -v error -i "$work/video.mp4" -frames:v 100 -f framemd5 "$work/frames.md5"
unique=$(grep -v '^#' "$work/frames.md5" | awk '{print $NF}' | sort -u | wc -l)
[[ $unique -gt 10 ]]
echo "PASS: 1440p EGL pixels, $unique distinct decoded frames, external NVENC, nonblocking full pool, game survives stop"
