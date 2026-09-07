#!/usr/bin/env bash
set -euo pipefail
exec tail -n 100 -F "${XDG_STATE_HOME:-$HOME/.local/state}/wm/session.log"
