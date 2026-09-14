#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
[[ -n ${DISPLAY:-} && -x $root/../../target/release/luma-game-capture-muxer ]] || {
    echo "SKIP: generic injection test needs DISPLAY and the release muxer" >&2
    exit 77
}
work=$(mktemp -d -- "${XDG_RUNTIME_DIR:-/tmp}/luma-generic-inject-test.XXXXXX")
game_pid= helper_pid=
cleanup() {
    [[ -n ${helper_pid:-} ]] && kill -TERM "$helper_pid" 2>/dev/null || true
    [[ -n ${helper_pid:-} ]] && wait "$helper_pid" 2>/dev/null || true
    [[ -n ${game_pid:-} ]] && kill -TERM "$game_pid" 2>/dev/null || true
    [[ -n ${game_pid:-} ]] && wait "$game_pid" 2>/dev/null || true
    if [[ -n ${LUMA_KEEP_GENERIC_INJECT_TEST:-} ]]; then
        echo "generic injection test files: $work" >&2
    else
        rm -rf -- "$work"
    fi
}
trap cleanup EXIT
cc -O2 -pipe -Wall -Wextra -Wpedantic -Werror -o "$work/game" \
    "$root/tests/generic-inject-glx.c" -lX11 -lGL
"$work/game" >"$work/game.log" 2>&1 & game_pid=$!
for _ in $(seq 1 300); do
    grep -q READY "$work/game.log" 2>/dev/null && break
    sleep 0.01
done
grep -q READY "$work/game.log" || { cat "$work/game.log" >&2; exit 1; }
output=$work/generic.mp4
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
packets=$(ffprobe -v error -select_streams v:0 -count_packets \
    -show_entries stream=nb_read_packets -of default=nw=1:nk=1 "$output")
[[ $packets =~ ^[0-9]+$ && $packets -ge 30 ]]
wait "$game_pid"
game_pid=
echo "PASS: generic ptrace/ELF OpenGL injection recorded $packets packets and left the game alive"
