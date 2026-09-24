#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_dir/target/performance}"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-cpu=native"

exec cargo build --workspace --profile performance --locked --offline "$@"
