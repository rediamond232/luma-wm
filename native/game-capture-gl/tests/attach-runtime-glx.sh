#!/usr/bin/env bash
set -euo pipefail

hook_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime=${XDG_RUNTIME_DIR:-/tmp}
display=${DISPLAY:-}
java_bin=${LUMA_GAME_CAPTURE_TEST_JAVA:-$(command -v java || true)}
javac_bin=${LUMA_GAME_CAPTURE_TEST_JAVAC:-$(command -v javac || true)}
attacher=${LUMA_GAME_CAPTURE_TEST_ATTACHER:-$hook_dir/luma-game-attach}
if [[ -z $display || ! -x $java_bin || ! -x $javac_bin ]] || \
   ! command -v ffprobe >/dev/null; then
    echo "SKIP: running-JVM .so injection test needs DISPLAY, java, javac, and ffprobe" >&2
    exit 77
fi
if ! [[ -x $hook_dir/../../target/release/luma-game-capture-muxer ]]; then
    echo "SKIP: build target/release/luma-game-capture-muxer first" >&2
    exit 77
fi

workdir=$(mktemp -d -- "$runtime/luma-jvm-attach-test.XXXXXX")
java_pid=
attach_pid=
cleanup() {
    if [[ -n ${attach_pid:-} ]] && kill -0 "$attach_pid" 2>/dev/null; then
        kill -TERM "$attach_pid" 2>/dev/null || true
        wait "$attach_pid" 2>/dev/null || true
    fi
    if [[ -n ${java_pid:-} ]] && kill -0 "$java_pid" 2>/dev/null; then
        kill -TERM "$java_pid" 2>/dev/null || true
        wait "$java_pid" 2>/dev/null || true
    fi
    rm -rf -- "$workdir"
}
trap cleanup EXIT

"$javac_bin" --release 8 -d "$workdir" "$hook_dir/tests/LumaAttachFixture.java"
cc -O2 -pipe -fPIC -shared -Wall -Wextra -Wpedantic -Werror \
    -o "$workdir/liblwjgl64.so" "$hook_dir/tests/jvm-glx-fixture.c" -lX11 -lGL

"$java_bin" -cp "$workdir" LumaAttachFixture "$workdir/liblwjgl64.so" 6 \
    >"$workdir/java.log" 2>&1 &
java_pid=$!
for _ in $(seq 1 200); do
    grep -q '^READY$' "$workdir/java.log" 2>/dev/null && break
    kill -0 "$java_pid" 2>/dev/null || break
    sleep 0.01
done
grep -q '^READY$' "$workdir/java.log" || {
    cat "$workdir/java.log" >&2
    echo "FAIL: JVM GLX fixture did not become ready" >&2
    exit 1
}

output=$workdir/attached.mp4
LUMA_GAME_CAPTURE_MUXER="$hook_dir/../../target/release/luma-game-capture-muxer" \
LUMA_GAME_CAPTURE_GL_HOOK="$hook_dir/build/libluma-game-capture-gl.so" \
LUMA_GAME_CAPTURE_DEBUG=1 \
    "$attacher" --pid "$java_pid" --output "$output" \
    --fps "${LUMA_GAME_CAPTURE_TEST_FPS:-240}" --quality 30 \
    >"$workdir/attach.log" 2>&1 &
attach_pid=$!
for _ in $(seq 1 500); do
    grep -Eq 'Luma (attached|injected) OpenGL capture' "$workdir/attach.log" 2>/dev/null && break
    kill -0 "$attach_pid" 2>/dev/null || break
    sleep 0.01
done
grep -Eq 'Luma (attached|injected) OpenGL capture' "$workdir/attach.log" || {
    cat "$workdir/attach.log" >&2
    echo "FAIL: attach helper exited before attaching" >&2
    exit 1
}
sleep 2
kill -TERM "$attach_pid"
wait "$attach_pid"
attach_pid=
kill -0 "$java_pid" 2>/dev/null || {
    cat "$workdir/java.log" >&2
    echo "FAIL: stopping capture also stopped the target JVM" >&2
    exit 1
}
wait "$java_pid"
java_pid=

packets=$(ffprobe -v error -select_streams v:0 -count_packets \
    -show_entries stream=nb_read_packets -of default=nw=1:nk=1 "$output")
[[ $packets =~ ^[0-9]+$ && $packets -ge 30 ]] || {
    echo "FAIL: attached JVM recording has only ${packets:-zero} packets" >&2
    exit 1
}
echo "PASS: runtime .so injection recorded $packets real packets, released GL/NVENC, and left the JVM running"
