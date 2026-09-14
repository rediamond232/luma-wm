#!/usr/bin/env bash
# Regression: stopping the recorder sidecar must not terminate the launched
# game. The fixture uses no GL or NVENC; it verifies the process boundary that
# the real wrapper relies on.
set -euo pipefail

if [[ ${1:-} == --fake-game ]]; then
    printf '%s\n' "$BASHPID" >"$2"
    while :; do sleep 1; done
fi

if [[ ${LUMA_TEST_FAKE_MUXER:-} == 1 ]]; then
    # The wrapper only needs a private listener for this ownership test.
    socket=
    while (($#)); do
        case "$1" in
            --socket) socket=$2; shift 2 ;;
            *) shift ;;
        esac
    done
    [[ $socket = /* ]] || exit 64
    python3 -c '
import signal
import socket
import sys
listener = socket.socket(socket.AF_UNIX)
listener.bind(sys.argv[1])
listener.listen(1)
signal.pause()
' "$socket"
    exit 0
fi

root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
workdir=$(mktemp -d "${TMPDIR:-/tmp}/luma-game-stop.XXXXXX")
record_pid=
game_pid=

cleanup() {
    if [[ -n ${record_pid:-} ]] && kill -0 "$record_pid" 2>/dev/null; then
        kill -TERM "$record_pid" 2>/dev/null || true
        wait "$record_pid" 2>/dev/null || true
    fi
    if [[ -n ${game_pid:-} ]] && kill -0 "$game_pid" 2>/dev/null; then
        kill -TERM "$game_pid" 2>/dev/null || true
        wait "$game_pid" 2>/dev/null || true
    fi
    rm -rf -- "$workdir"
}
trap cleanup EXIT

LUMA_GAME_CAPTURE_MUXER="$0" \
LUMA_GAME_CAPTURE_GL_HOOK="$0" \
LUMA_TEST_FAKE_MUXER=1 \
    "$root/luma-game-record-gl" --output "$workdir/recording.mp4" --fps 120 -- \
    "$0" --fake-game "$workdir/game.pid" >"$workdir/record.log" 2>&1 &
record_pid=$!

for _ in $(seq 1 500); do
    [[ -s $workdir/game.pid ]] && break
    sleep 0.01
done
[[ -s $workdir/game.pid ]] || { cat "$workdir/record.log" >&2; exit 1; }
game_pid=$(<"$workdir/game.pid")
[[ $game_pid =~ ^[1-9][0-9]*$ ]] || exit 1
kill -0 "$game_pid"

# This mirrors RecorderController: signal only the wrapper PID.
kill -TERM "$record_pid"
set +e
wait "$record_pid"
status=$?
set -e
record_pid=
[[ $status -eq 0 ]] || { echo "wrapper exited $status" >&2; exit 1; }

# Give the wrapper cleanup a moment. A surviving target proves that neither
# the trap nor session teardown propagated termination into the game.
sleep 0.1
kill -0 "$game_pid" || { echo 'FAIL: stopping recorder terminated the game' >&2; exit 1; }
game_session=$(ps -o sid= -p "$game_pid" | tr -d ' ')
wrapper_session=$(ps -o sid= -p "$$" | tr -d ' ')
[[ $game_session != "$wrapper_session" ]] || { echo 'FAIL: game did not enter an isolated session' >&2; exit 1; }

echo "PASS stop boundary: recorder exited while game PID $game_pid remained alive in session $game_session" >&2
