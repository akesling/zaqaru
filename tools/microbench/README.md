# Execution benchmarks

Run on **x86-64 Linux** with GCC/static libc, Python 3.11+, Rust (including
`wasm32-unknown-unknown`), LLVM `wasm-ld`, and `taskset` installed:

```sh
python3 tools/microbench/measure.py --output benchmark-results/before.json
# Make the engine change, then run the same workload on the same machine.
python3 tools/microbench/measure.py --output benchmark-results/after.json
python3 tools/microbench/compare.py benchmark-results/before.json benchmark-results/after.json
```

The runner rebuilds Zaqaru and the guest, bakes into a fresh temporary directory,
and compares native Linux, Wasm with `--no-bytecode`, and Wasm with bytecode.
All 15 kernels run by default. For a shorter experiment use
`--kernels mixed loads stores --repeats 3`; use the same arguments on both
revisions. `--core` selects a CPU from the process's allowed affinity set.
Each repetition alternates S/2S and rotates engine order. Native scales are
calibrated to at least 0.3 seconds. Every checksum is checked, all raw timings
are saved, and nonpositive timing differences fail rather than produce ratios.
The difference of minima estimates steady execution cost; it does not eliminate
measurement noise. Repeat apparent regressions before drawing conclusions.

Results include the commit, dirty status, executable hash, compiler, machine,
CPU affinity, environment, and timestamp. Keep separate output files: the
selected output path is replaced. `compare.py` checks environment/workload
compatibility and exits nonzero for a slowdown above `--threshold` (default
0.10 = 10%). Use a dedicated machine for performance gates. Shared CI runners
provide smoke coverage and result artifacts, not stable timing thresholds.

## macOS / Docker

```sh
./tools/microbench/linux.sh --kernels mixed loads stores --repeats 3
```

Requires a running Docker daemon. This builds and runs in an amd64 Linux
container, with a separate Cargo target volume; results go to
`benchmark-results/results.json`. On Apple Silicon, Docker's amd64 emulation
adds another execution layer. These results are useful for checking the harness,
but must not be treated as native x86 performance or compared to bare-metal
Linux baselines. The JSON explicitly identifies the Docker environment.

## Application latency

On x86-64 Linux with Docker running:

```sh
bash tools/microbench/django-latency.sh
```

The script exports the current Docker image every time and rebakes the module.
The client saves request samples and metadata under `/tmp/microbench/` and
checks nonempty HTTP 200 responses in cold, warmup, sequential and concurrent
phases. Wasm readiness observes process CPU use without issuing requests, so
`cold_ms` is the first request. Native readiness polls HTTP and therefore warms
the application: its `cold_ms` is null, with a separately labelled
`post_readiness_ms`. Readiness times use different detection methods and are
not precise, directly comparable boot times. The script starts fresh servers for both interpreter and bytecode measurements. Application baselines are inspected separately
from the microbenchmark comparison tool.

## Harness checks

```sh
python3 -m unittest discover -s tools/microbench -p 'test_*.py'
```

The native Rust example is an additional engine diagnostic, not a Wasmtime
performance result:

```sh
cargo run --release -p zaqaru-cpu --example bytecode_bench -- 100000
```
