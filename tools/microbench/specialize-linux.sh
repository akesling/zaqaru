#!/usr/bin/env bash
set -euo pipefail
iterations=${1:-10000000}
[[ $iterations =~ ^[1-9][0-9]*$ ]] || { echo 'Iterations must be positive' >&2; exit 1; }
archive=benchmark-results/weval-v0.5.0-x86_64-linux.tar.xz
if [[ ! -f $archive ]]; then
    curl --fail --location --output "$archive" \
        https://github.com/bytecodealliance/weval/releases/download/v0.5.0/weval-v0.5.0-x86_64-linux.tar.xz
fi
echo "2f1746e7babe6e4436a401ceb1a2a0685ab620ead99fd544b7de23ad356e18ab  $archive" | sha256sum --check
tar -xf "$archive" -C benchmark-results
python3 tools/microbench/specialize-reference.py
CARGO_TARGET_DIR=/work/target/specialize-reference cargo build --locked --release \
    --manifest-path benchmark-results/specialize-reference/Cargo.toml \
    -p zaqaru-cpu --example specialize --features specialize --target wasm32-unknown-unknown
benchmark-results/weval-v0.5.0-x86_64-linux/weval weval -w \
    -i target/specialize-reference/wasm32-unknown-unknown/release/examples/specialize.wasm \
    -o benchmark-results/specialize-reference.wasm > benchmark-results/weval-reference.log 2>&1
cargo build --locked --release -p zaqaru-cpu --example specialize \
    --features specialize --target wasm32-unknown-unknown
start=$SECONDS
benchmark-results/weval-v0.5.0-x86_64-linux/weval weval -w --show-stats \
    -i target/wasm32-unknown-unknown/release/examples/specialize.wasm \
    -o benchmark-results/specialized.tmp.wasm > benchmark-results/weval.log 2>&1
specialization_seconds=$((SECONDS - start))
mv benchmark-results/specialized.tmp.wasm benchmark-results/specialized.wasm
cargo build --locked --release -p zaqaru-host --example specialize_bench
taskset -c 0 target/release/examples/specialize_bench \
    benchmark-results/specialized.wasm benchmark-results/specialize-reference.wasm \
    "$iterations" | tee benchmark-results/specialize.tmp.csv
mv benchmark-results/specialize.tmp.csv benchmark-results/specialize.csv
python3 tools/microbench/specialize-report.py "$iterations" "$specialization_seconds"
