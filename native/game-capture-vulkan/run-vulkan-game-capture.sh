#!/usr/bin/env bash
# Launch one game with Luma's explicit Vulkan present observer enabled.
set -euo pipefail

if [ "$#" -lt 3 ] || [ "$1" != "--socket" ] || [ "$3" != "--" ]; then
  echo "Usage: $0 --socket /path/to/luma-events.sock -- game [args...]" >&2
  exit 64
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
library=${LUMA_VULKAN_CAPTURE_LAYER_LIBRARY:-"$script_dir/build/libluma-vulkan-capture-layer.so"}
if [ ! -f "$library" ]; then
  echo "Luma Vulkan capture layer missing: $library (run make first)" >&2
  exit 66
fi

runtime_dir=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/luma-vk-layer.XXXXXX")
cleanup() { rm -rf "$runtime_dir"; }
trap cleanup EXIT
escaped_library=$(printf '%s' "$library" | sed 's/[&|]/\\&/g')
sed "s|@LIBRARY_PATH@|$escaped_library|" "$script_dir/luma_game_capture_layer.json.in" > "$runtime_dir/luma_game_capture_layer.json"

export LUMA_GAME_CAPTURE_SOCKET=$2
export VK_LAYER_PATH="$runtime_dir${VK_LAYER_PATH:+:$VK_LAYER_PATH}"
export VK_INSTANCE_LAYERS="VK_LAYER_LUMA_game_capture${VK_INSTANCE_LAYERS:+:$VK_INSTANCE_LAYERS}"
shift 3
"$@"
