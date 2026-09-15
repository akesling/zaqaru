#!/bin/sh
# Build and cache the complete validated demo, including its compressed snapshot.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
exec python3 "$repo/web/demo_build.py" "$@"
