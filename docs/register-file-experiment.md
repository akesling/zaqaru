# Bytecode register file experiment (2026-09-13)

Baseline: `57ad2ff` (the benchmark-harness branch). Candidate: the same engine
with the bytecode interpreter’s local register array padded from 24 to 32 slots.

The operand encoding uses five-bit indices. A 32-slot array makes every decoded
general-register index provably in bounds, allowing the compiler to remove
per-operand bounds checks. The transpiler still allocates only 24 slots, and
only the 16 architectural registers are copied back. This adds 64 bytes to the
local array and changes neither the bytecode format nor guest state.

## C workloads

Local Docker `linux/amd64` on Apple Silicon, CPU affinity 0, Wasmtime from the
locked workspace, Rust 1.98.1, GCC 12.2.0, Python timing harness 3.11.2.
These results include Docker’s amd64 emulation; they are not native x86-64
measurements and do not establish gains on other hosts.

Five repetitions per workload and scale, alternating the baseline and
candidate executables and reversing their order on successive repetitions.
Each cost is `min(2S) - min(S)` divided by S. All checksums match native Linux
and all baseline/candidate instruction counts match.

The geometric mean across all 15 workloads is **1.149×**
(14.9% higher throughput, 13.0% less time).
Using medians of paired S/2S differences instead gives 1.151×. This is an
equally weighted benchmark aggregate, not a claim that every program improves
by 10%. Strings and floating point remain within 1% of baseline.

| Workload | Baseline ns/unit | Candidate ns/unit | Speedup |
| --- | ---: | ---: | ---: |
| mixed | 229.1 | 215.8 | 1.062× |
| nops | 297.6 | 252.1 | 1.180× |
| regmov | 300.0 | 245.4 | 1.222× |
| regadd | 389.2 | 361.2 | 1.077× |
| loads | 434.8 | 295.3 | 1.472× |
| stores | 499.8 | 403.7 | 1.238× |
| alu | 103.0 | 89.0 | 1.157× |
| memory_sequential | 124,705,521.0 | 103,493,104.0 | 1.205× |
| memory_random | 31.4 | 28.5 | 1.103× |
| calls | 9,129,355.0 | 7,500,050.3 | 1.217× |
| branches | 121.7 | 98.3 | 1.238× |
| string | 934,093.0 | 940,794.8 | 0.993× |
| float | 475.0 | 478.2 | 0.993× |
| syscalls | 674.0 | 652.0 | 1.034× |
| alloc | 5,656.7 | 4,978.0 | 1.136× |

## Validation and reproduction

All 51 CPU unit tests and 27 bytecode differential tests passed. The latter
now explicitly disable acceleration on the reference path. The harness also
has six passing tests, including a check that A/B runs use matching baked
modules and alternate execution order.

Use `tools/microbench/experiment.sh save baseline` with the baseline engine
sources, then `save candidate` with the candidate sources, followed by:

```sh
./tools/microbench/experiment.sh ab --repeats 5
./tools/microbench/experiment.sh test
```

The full local samples are in `benchmark-results/ab.json`. Saved executable
SHA-256 hashes (the rebuilt baseline matches the first baseline exactly):

- Baseline: `17015da322db746ede6b129b073325e3ef551c1fa939ec931b1def8781ae3314`
- Candidate: `cf2c14d9c51cbe9bebfe7297dcba71452deaba1bb68b1ce3fa502ab917fdc5c8`

## Runtime coverage limit

Supplemental minimal-rootfs runs of Debian Python 3.11 and Python 3.12 Bookworm
both stopped during startup at the existing unimplemented `sysinfo` syscall
(number 99). No Python or Django speedup is claimed. The failing supplemental
harness is not part of this change; kernel compatibility work is separate.
