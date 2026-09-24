#!/usr/bin/env bash
# Reproduce Lunar's obs-gamecapture arrangement: the game runs with a capture
# shim preloaded (libobs_glcapture.so interposes glXSwapBuffers), and Luma's
# runtime injection must chain through it instead of reporting "no supported
# resolved GLX/EGL call site".
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
[[ -n ${DISPLAY:-} && -x $root/../../target/release/luma-game-capture-muxer ]] || {
    echo "SKIP: shim interposition test needs DISPLAY and the release muxer" >&2
    exit 77
}
work=$(mktemp -d -- "${XDG_RUNTIME_DIR:-/tmp}/luma-shim-interpose-test.XXXXXX")
game_pid= helper_pid=
cleanup() {
    [[ -n ${helper_pid:-} ]] && kill -TERM "$helper_pid" 2>/dev/null || true
    [[ -n ${helper_pid:-} ]] && wait "$helper_pid" 2>/dev/null || true
    [[ -n ${game_pid:-} ]] && kill -TERM "$game_pid" 2>/dev/null || true
    [[ -n ${game_pid:-} ]] && wait "$game_pid" 2>/dev/null || true
    if [[ -n ${LUMA_KEEP_SHIM_INTERPOSE_TEST:-} ]]; then
        echo "shim interposition test files: $work" >&2
    else
        rm -rf -- "$work"
    fi
}
trap cleanup EXIT
cc -O2 -pipe -Wall -Wextra -Wpedantic -Werror -fPIC -shared \
    -o "$work/libobs_glcapture.so" "$root/tests/shim_glcapture.c" -ldl
cc -O2 -pipe -Wall -Wextra -Wpedantic -Werror -o "$work/game" \
    "$root/tests/generic-inject-glx.c" -lX11 -lGL
LD_PRELOAD="$work/libobs_glcapture.so" "$work/game" >"$work/game.log" 2>&1 & game_pid=$!
for _ in $(seq 1 300); do
    grep -q READY "$work/game.log" 2>/dev/null && break
    sleep 0.01
done
grep -q READY "$work/game.log" || { cat "$work/game.log" >&2; exit 1; }
output=$work/shimmed.mp4
LUMA_GAME_CAPTURE_MUXER="$root/../../target/release/luma-game-capture-muxer" \
LUMA_GAME_CAPTURE_GL_HOOK="$root/build/libluma-game-capture-gl.so" \
LUMA_GAME_CAPTURE_INJECTOR="$root/../game-capture-inject/build/luma-game-inject" \
    "$root/luma-game-attach" --pid "$game_pid" --output "$output" --fps 240 --quality 30 \
    >"$work/attach.log" 2>&1 & helper_pid=$!
for _ in $(seq 1 500); do
    grep -q 'Luma injected OpenGL capture' "$work/attach.log" 2>/dev/null && break
    kill -0 "$helper_pid" 2>/dev/null || break
    sleep 0.01
done
grep -q 'Luma injected OpenGL capture' "$work/attach.log" || {
    cat "$work/attach.log" >&2
    exit 1
}
sleep 2
kill -TERM "$helper_pid"
wait "$helper_pid"
helper_pid=
kill -0 "$game_pid"
runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
grep -q 'chained through capture shim' "$runtime/luma-game-capture-inject-$game_pid.log" || {
    echo "expected the hook to chain through the preloaded shim" >&2
    cat "$runtime/luma-game-capture-inject-$game_pid.log" >&2
    exit 1
}
packets=$(ffprobe -v error -select_streams v:0 -count_packets \
    -show_entries stream=nb_read_packets -of default=nw=1:nk=1 "$output")
[[ $packets =~ ^[0-9]+$ && $packets -ge 30 ]]
wait "$game_pid"
game_pid=
echo "PASS: injection chained through preloaded capture shim, recorded $packets packets, game alive"
