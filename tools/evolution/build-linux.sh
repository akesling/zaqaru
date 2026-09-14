#!/usr/bin/env bash
set -euo pipefail
kernel=${1:-alu}
scale=${2:-4000000}
feature=${3:-evolution}
[[ $feature == evolution || $feature == regions ]] || exit 2
[[ $kernel =~ ^[a-z_]+$ && $scale =~ ^[1-9][0-9]*$ ]] || exit 2
baseline=benchmark-results/baseline-2x.zaqaru
if ! echo "cf2c14d9c51cbe9bebfe7297dcba71452deaba1bb68b1ce3fa502ab917fdc5c8  $baseline" | sha256sum --check --status; then
  (
    cd benchmark-results/evolution-baseline-source
    CARGO_TARGET_DIR=/work/target/evolution-baseline cargo build --locked --release -p zaqaru
  )
  cp target/evolution-baseline/release/zaqaru "$baseline"
fi
cargo build --locked --release -p zaqaru-guest --features "$feature" --target wasm32-unknown-unknown
cargo build --locked --release --manifest-path tools/evolution/Cargo.toml --target-dir /work/target
cargo build --locked --release --manifest-path benchmark-results/browser-compiler/Cargo.toml \
  --target wasm32-unknown-unknown --target-dir /work/target/browser-compiler
cp target/browser-compiler/wasm32-unknown-unknown/release/zaqaru_browser_compiler.wasm benchmark-results/browser-compiler.wasm
mkdir -p benchmark-results/evolution-root
gcc -O2 -static -o benchmark-results/evolution-root/init tools/microbench/bench.c -lm
target/release/zaqaru-evolution bake target/wasm32-unknown-unknown/release/libguest.a \
  benchmark-results/evolution-root benchmark-results/evolution-template.wasm /init "$kernel" "$scale"
benchmark-results/baseline-2x.zaqaru bake benchmark-results/evolution-root \
  -o benchmark-results/evolution-baseline.wasm -- /init "$kernel" "$scale"
