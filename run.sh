#!/usr/bin/env bash
# Start the subtitle server, then open http://localhost:8765 in Chrome or Edge.
# (The Windows app starts this itself through wsl.exe, where ~/.local/bin is not on PATH.)
export PATH="$HOME/.local/bin:$PATH"
command -v uv >/dev/null || { echo "uv が見つかりません。README の「入れ方」の手順で入れてください" >&2; exit 127; }
# Fewer glibc malloc arenas: 25-80 MB less memory for the engine, no measurable cost.
export MALLOC_ARENA_MAX="${MALLOC_ARENA_MAX:-2}"
cd "$(dirname "$0")" && exec uv run server.py "$@"
