#!/usr/bin/env bash
# Docker supplies the x86-64 Linux ABI the benchmark guest and native leg need.
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$SCRIPT_DIR/../.." && pwd)
docker build --platform linux/amd64 -t zaqaru-bench -f "$SCRIPT_DIR/Dockerfile" "$SCRIPT_DIR"
mkdir -p "$REPO/benchmark-results"
if [[ ${1:-} == --exec ]]; then
    shift
    entrypoint=$1
    shift
    exec docker run --rm --platform linux/amd64 \
        -v "$REPO:/work" -v zaqaru-bench-target:/work/target \
        -v zaqaru-bench-cargo:/usr/local/cargo/registry \
        --entrypoint "$entrypoint" zaqaru-bench "$@"
fi
docker run --rm --platform linux/amd64 \
    -v "$REPO:/work" -v zaqaru-bench-target:/work/target \
    -v zaqaru-bench-cargo:/usr/local/cargo/registry \
    zaqaru-bench --output /work/benchmark-results/results.json "$@"
