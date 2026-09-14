#!/usr/bin/env bash
set -euo pipefail

library=$1
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

output=$("$(dirname "$library")/../luma-game-capture-gl" --socket "$workdir/events.sock" -- \
    /usr/bin/env bash -c 'printf "%s|%s" "$LUMA_GAME_CAPTURE_SOCKET" "$LD_PRELOAD"')

[[ $output == "$workdir/events.sock|$library" ]]

# The record helper starts the launcher in the background. In that case Bash
# keeps `$$` from the parent shell, while BASHPID is the process that will
# exec the target. The PID file and hook guard must name the latter.
pid_file=$workdir/expected.pid
"$(dirname "$library")/../luma-game-capture-gl" --socket "$workdir/events.sock" \
    --expected-pid-file "$pid_file" -- /usr/bin/env bash -c \
    'printf "%s|%s" "$LUMA_GAME_CAPTURE_TARGET_PID" "$BASHPID"' \
    >"$workdir/pid-output" &
launcher_pid=$!
wait "$launcher_pid"
pid_output=$(cat "$workdir/pid-output")
pid_from_file=$(cat "$pid_file")
[[ $pid_output == "$pid_from_file|$pid_from_file" ]]

readelf --dyn-syms --wide "$library" | grep -q ' glXSwapBuffers$'
readelf --dyn-syms --wide "$library" | grep -q ' eglSwapBuffers$'
