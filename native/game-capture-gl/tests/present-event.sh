#!/usr/bin/env bash
set -euo pipefail

library=$1
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
hook_dir=$(cd -- "$source_dir/.." && pwd)
workdir=$(mktemp -d)
socket_path=$workdir/events.sock
ready_path=$workdir/ready
result_path=$workdir/result
trap 'rm -rf "$workdir"' EXIT

cc -O2 -fPIC -shared -o "$workdir/libfake-glx.so" "$source_dir/fake_glx.c"
cc -O2 -o "$workdir/call-glx" "$source_dir/call_glx.c" -ldl

SOCKET_PATH=$socket_path READY_PATH=$ready_path RESULT_PATH=$result_path python3 - <<'PY' &
import os
import socket
import struct

sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
sock.bind(os.environ["SOCKET_PATH"])
sock.settimeout(0.25)
open(os.environ["READY_PATH"], "w").close()
packet = sock.recv(64)
try:
    unexpected = sock.recv(64)
except socket.timeout:
    unexpected = None
if unexpected is not None:
    raise SystemExit("capture hook emitted an event for a non-owner surface")
magic, version, length, api, sequence, stamp, display, surface, width, height, reserved = struct.unpack("=QHHIQQQQIIQ", packet)
with open(os.environ["RESULT_PATH"], "w") as result:
    result.write(f"{magic:x} {version} {length} {api} {sequence} {display} {surface} {width} {height} {reserved}")
PY
receiver_pid=$!

for _ in $(seq 1 100); do
    [[ -e $ready_path ]] && break
    sleep 0.01
done
[[ -e $ready_path ]]

LD_PRELOAD="$workdir/libfake-glx.so" "$hook_dir/luma-game-capture-gl" --socket "$socket_path" -- "$workdir/call-glx" >/dev/null
wait "$receiver_pid"

[[ $(cat "$result_path") == "31504c414d554c 1 64 1 1 0 7 0 0 0" ]]
