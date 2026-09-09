#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --workspace --locked "$@"
printf '\nBuilt: %s/target/debug/{wm,wmctl,wm-shell,wm-shell-sctk}\n' "$PWD"
