#!/usr/bin/env bash
# Repeat recordings on ONE process without re-injection: attach, stop, then
# attach again to the same PID. The resident hook re-arms itself on its own
# present thread; the injector must not run a second time.
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
[[ -n ${DISPLAY:-} && -x $root/../../target/release/luma-game-capture-muxer ]] || {
    echo "SKIP: re-record test needs DISPLAY and the release muxer" >&2
    exit 77
}
work=$(mktemp -d -- "${XDG_RUNTIME_DIR:-/tmp}/luma-rerecord-test.XXXXXX")
game_pid= helper_pid=
cleanup() {
    [[ -n ${helper_pid:-} ]] && kill -TERM "$helper_pid" 2>/dev/null || true
    [[ -n ${helper_pid:-} ]] && wait "$helper_pid" 2>/dev/null || true
    [[ -n ${game_pid:-} ]] && kill -TERM "$game_pid" 2>/dev/null || true
    [[ -n ${game_pid:-} ]] && wait "$game_pid" 2>/dev/null || true
    if [[ -n ${LUMA_KEEP_RERECORD_TEST:-} ]]; then
        echo "re-record test files: $work" >&2
    else
        rm -rf -- "$work"
    fi
}
trap cleanup EXIT
cc -O2 -pipe -Wall -Wextra -Wpedantic -Werror -o "$work/game" \
    "$root/tests/generic-inject-glx.c" -lX11 -lGL
LUMA_TEST_RUNTIME_S=30 "$work/game" >"$work/game.log" 2>&1 & game_pid=$!
for _ in $(seq 1 300); do
    grep -q READY "$work/game.log" 2>/dev/null && break
    sleep 0.01
done
grep -q READY "$work/game.log" || { cat "$work/game.log" >&2; exit 1; }

attach() {
    local output=$1 log=$2
    LUMA_GAME_CAPTURE_MUXER="$root/../../target/release/luma-game-capture-muxer" \
    LUMA_GAME_CAPTURE_GL_HOOK="$root/build/libluma-game-capture-gl.so" \
    LUMA_GAME_CAPTURE_INJECTOR="$root/../game-capture-inject/build/luma-game-inject" \
        "$root/luma-game-attach" --pid "$game_pid" --output "$output" --fps 240 --quality 30 \
        >"$log" 2>&1 & helper_pid=$!
    for _ in $(seq 1 500); do
        grep -q 'Luma injected OpenGL capture' "$log" 2>/dev/null && break
        kill -0 "$helper_pid" 2>/dev/null || break
        sleep 0.01
    done
    grep -q 'Luma injected OpenGL capture' "$log" || {
        cat "$log" >&2
        return 1
    }
}

attach "$work/first.mp4" "$work/attach1.log"
sleep 3
kill -TERM "$helper_pid"
wait "$helper_pid"
helper_pid=
grep -q 'already resident' "$work/attach1.log" && {
    echo "first attach must inject, not re-arm" >&2
    exit 1
}

attach "$work/second.mp4" "$work/attach2.log"
grep -q 'already resident' "$work/attach2.log" || {
    echo "second attach must reuse the resident hook without ptrace" >&2
    cat "$work/attach2.log" >&2
    exit 1
}
sleep 3
kill -TERM "$helper_pid"
wait "$helper_pid"
helper_pid=
kill -0 "$game_pid"
for output in "$work/first.mp4" "$work/second.mp4"; do
    packets=$(ffprobe -v error -select_streams v:0 -count_packets \
        -show_entries stream=nb_read_packets -of default=nw=1:nk=1 "$output")
    [[ $packets =~ ^[0-9]+$ && $packets -ge 30 ]] || {
        echo "expected >= 30 packets in $output, got ${packets:-none}" >&2
        exit 1
    }
    echo "recorded $packets packets in $output"
done
wait "$game_pid"
game_pid=
echo "PASS: two recordings on one process without re-injection, game alive"
