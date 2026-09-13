#!/bin/bash
# The same OCI image two ways: `docker run` and `zaqaru run`, same client.
#
# What is deliberately *not* controlled: the native container gets the whole
# machine and the module is single-threaded by construction, because that is
# the comparison worth having. The question is not "how do these compare per
# core", it is "what does this cost me".
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=${ZAQARU_REPO:-$(cd "$SCRIPT_DIR/../.." && pwd)}
if [[ $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
    echo "Run this harness in an x86-64 Linux environment; see tools/microbench/README.md" >&2
    exit 1
fi
OUT=${ZAQARU_DEMO_OUT:-/tmp/zaqaru-demo}
NATIVE_PORT=${NATIVE_PORT:-8091}
WASM_PORT=${WASM_PORT:-8090}
mkdir -p "$OUT"

container=""
docker_id=""
cleanup() {
    [ -n "$container" ] && kill "$container" 2>/dev/null || true
    [ -n "$docker_id" ] && docker rm -f "$docker_id" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

echo "== building the image (cached) =="
docker build -q -t hello-django "$REPO/demo/hello-django" >/dev/null
docker save hello-django:latest -o "$OUT/hello-django.tar"

echo "== baking the module =="
cargo build --manifest-path "$REPO/Cargo.toml" --release --quiet -p zaqaru
"$REPO/target/release/zaqaru" bake "$OUT/hello-django.tar" -o "$OUT/hello-django.wasm"
ls -la "$OUT/hello-django.wasm" | awk '{printf "module: %.1f MB\n", $5/1048576}'

echo
echo "== native: docker run =="
docker_id=$(docker run --rm -d -p "$NATIVE_PORT:80" hello-django)
python3 "$REPO/tools/microbench/latency.py" native "$NATIVE_PORT" 120
docker rm -f "$docker_id" >/dev/null; docker_id=""

echo
for mode in interpreter bytecode; do
    echo "== wasm: $mode =="
    flags=()
    if [[ $mode == interpreter ]]; then flags+=(--no-bytecode); fi
    "$REPO/target/release/zaqaru" run "$OUT/hello-django.wasm" -p "$WASM_PORT:80" \
        --seed 1 "${flags[@]}" >"$OUT/wasm.$mode.log" 2>&1 &
    container=$!
    python3 "$REPO/tools/microbench/latency.py" "wasm.$mode" "$WASM_PORT" 900 "$container"
    kill "$container" 2>/dev/null || true
    wait "$container" 2>/dev/null || true
    container=""
    grep -E 'compiled|instructions in' "$OUT/wasm.$mode.log" || true
done
