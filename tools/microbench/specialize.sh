#!/usr/bin/env bash
# Manual experiment only. Uses the same local amd64 Docker environment as A/B.
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
exec "$SCRIPT_DIR/experiment.sh" exec bash tools/microbench/specialize-linux.sh "$@"
