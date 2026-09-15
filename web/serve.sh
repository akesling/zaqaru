#!/bin/sh
# Serve cached demo artifacts, building only when needed.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
exec python3 "$repo/web/serve.py" "$@"
