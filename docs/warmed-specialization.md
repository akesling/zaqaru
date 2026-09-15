# Warmed specialization after PR #4

Research checkpoint based on `60d2717`, measured locally on 2026-09-14.
The milestone is a reusable warmed-browser benchmark, emitted-code inspection,
and two opt-in compiler experiments. **This checkpoint delivers no additional
execution speedup over PR #4, and normal container execution is unchanged.**
The warmed measurements below characterize existing PR #4 specialization.
A general 10× OCI speedup is not established, and normal execution does not
automatically use this tier.

## What changed in the measurement

The earlier continuation experiment compiles a successor and measures its first
execution. That phase can include lazy machine-code compilation and execution
in a browser's baseline compiler. It does not characterize repeatedly used
optimized code. V8 documents that optimized Wasm does not replace an already
executing stack frame; later calls can use the new code instead:
<https://v8.dev/docs/wasm-compilation-pipeline>.

The new harness captures a successor only after the existing demo validates it
against its interpreter continuation. Each trial instantiates a fresh copy of
that frozen state, with fresh local mounts, using the **same WebAssembly.Module**.
It verifies output, exit status, retirement, process state and compiled/region
counters on every trial. No external requests are sent during disposable warmups.
Module compilation, instance preparation, first execution, warmup trials and
measured trials are recorded separately. Warmup does not certify a specific
compiler tier; no browser compiler flags are required.

An optional historical baseline boots to its recorded warmup boundary outside
the execution timer. Its tail runs through the historical engine, rather than
the experimental interpreter with discovery overhead. Variants alternate order
on successive rounds. Ratios are normalized by remaining retired instructions
if the baseline's boundary differs by at most one scheduler quantum. This small
normalization is an approximation, not an exact checkpoint pairing.

## Chrome results

Chrome 152 on local macOS, ordinary browser settings, eight disposable warmup
trials and nine measured trials per variant. These are two static C workloads
inside baked containers, not a representative OCI application corpus. Independent
browser sessions and additional hardware are still needed for broader estimates.

| Workload | Historical tail, median | Combined specialization tail, median | Median paired speedup |
| --- | ---: | ---: | ---: |
| `calls 40` | 113.1 ms | 13.8 ms | **8.19×** |
| `mixed 4000000` | 334.9 ms | 31.9 ms | **10.51×** |

The combined specialization is PR #4's `stack-forwarding,virtual-flags` mode.
The table measures its warmed behavior; it is **not an incremental gain from a
new compiler optimization in this checkpoint**. The calls comparison executes
identical remaining retirement. Mixed normalizes the historical warmup of
10,109,329 instructions against the successor's 10,010,174; both finish at
68,022,187 instructions.

The same session's first combined-successor tails were 69.1 ms for calls and
81.0 ms for mixed. A separate single-variant Chrome check went from 71.1 ms on
first use to a 13.7 ms warmed median on calls. Node 25.9.0 likewise settled near
18 ms after its first few trials. This confirms that ordinary module reuse can
expose much of the headroom first observed with diagnostic compiler flags.

Capture still spends approximately **3–4 seconds** generating and activating a
successor locally, with larger outliers. Those costs, initial guest warmup and
disposable benchmark warmups are excluded from the speedup table. These short
jobs remain slower when specialization transition cost is included. Fresh
instances took about 0.4–0.5 ms for the warmed successors in the final Chrome
sessions; that is also outside the execution-tail ratios.

Raw results are in `benchmark-results/warm-calls-final-browser.json` and
`benchmark-results/warm-mixed-final-browser.json`. Each includes all samples,
origin measurements, completed-work checks and artifact hashes. Final hashes:

| Artifact | SHA-256 |
| --- | --- |
| Calls historical | `d921118fafdfe01423743957e6a5cae69a45ad1f34d34e6b5e4b7df38fe58c2d` |
| Calls combined | `de42252c969a5d0bd377f27f10b0443fc4613334d578b1f5051f553e8a1bccd8` |
| Mixed historical | `1be79cdef011c8909d8ada88afb654b7ad1249116cc4546938946a853ae75c23` |
| Mixed combined | `b24bf846a38f5b964e87c5e74be24be5c950920664439b8d6d85c2bdf8288368` |

## Generated code and the follow-up experiments

The new `profile` command inventories static Wasm code and can print a function's
callees, opcode histogram and byte-offset disassembly. Node's optional CPU
profile runs separately after timing, because profiling can alter engine tiering.
In the initial calls CPU profile, 1,114 of 1,303 samples were in the specialized
region; sampling also included instance setup and garbage collection.

The calls region contains considerable cold exit handling. Counts below are
static operators, including cold paths, and do not measure dynamic guest-memory
traffic or prove native register spills.

| Calls region | Body bytes | Declared locals | Wasm operators | Static stores |
| --- | ---: | ---: | ---: | ---: |
| Existing combined | 231,052 | 1,204 | 94,162 | 10,574 |
| `shared-exit` added | 200,111 | 1,826 | 86,080 | 870 |
| `direct-transfers` added | 230,766 | 1,204 | 94,058 | 10,548 |

**Shared exit.** The opt-in feature makes specialization exits converge at one
compiler context and share the state-materialization epilogue. It preserves
the normal engine's returns. It removes approximately 92% of static stores but
increases the number of locals. In a nine-pair Chrome comparison, the combined
control measured 13.5 ms and shared exit 14.1 ms, a median incremental ratio of
**0.957×**. A smaller static body did not improve warm execution.

That comparison also re-evaluated PR #4's combined features against its
permission-fast-path checkpoint: 13.5 ms versus 19.8 ms, **1.459×** median paired
improvement when warmed. This is separate from the historical-baseline ratios.
Raw samples are in `benchmark-results/warm-exits-browser.json`.

**Direct transfers.** The opt-in feature recognizes an adjacent constant
load/transfer and links it to the region root. It leaves the original constant
load, target register, retirement bit and instruction offsets intact. It rejects
transfers that another branch can enter without executing the constant load.
Budget and dirty-code checks still happen before the jump, and the existing
member-identity validation remains required at region entry.

In the final Chrome sessions it measured 14.1 ms on calls and 32.1 ms on mixed.
Relative to the combined controls, median paired ratios were **0.986×** and
**0.994×**. There is no demonstrated improvement. Both new features remain off
by default and are retained as reproducible experiments.

Two broader attempts were rejected before timing: nested resolver contexts
produced weval's “PC is a runtime value” error; unrestricted links between region
members exhausted compiler memory during residual control-flow restructuring.
The final direct-transfer experiment therefore links only to the region entry.
Arbitrary CFG edges need an explicit restructuring strategy and compilation
resource limits; simply bypassing the directory is not sufficient.

## Reproduction

Use the preparation described in [tools/evolution](../tools/evolution/README.md).
Docker builds amd64 fixtures and Wasm archives; Node and Chrome execute Wasm
locally. The capture compiler is itself the existing Wasm optimizer Block.

```sh
bash tools/evolution/run.sh build calls 40 stack-forwarding,virtual-flags
bash tools/evolution/run.sh capture benchmark-results/evolution-template.wasm \
  benchmark-results/warm-control 22 benchmark-results/evolution-baseline.wasm
bash tools/evolution/run.sh warm-browser benchmark-results/warm-browser.json \
  benchmark-results/warm-control/baseline.json benchmark-results/warm-control/manifest.json \
  --warmups=8 --samples=9
bash tools/evolution/run.sh warm-node benchmark-results/warm-node.json \
  benchmark-results/warm-control/manifest.json --profile
bash tools/evolution/run.sh profile benchmark-results/warm-control/successor.wasm
```

Replace the workload with `mixed 4000000` for the second case. To compare a new
feature, add `shared-exit` or `direct-transfers` to the build's comma-separated
features and capture into a **different directory**. Supply both manifests to
the warm benchmark. The first manifest is the control. Function indices can
change between builds; read the profile inventory before requesting disassembly
with `profile MODULE FUNCTION_INDEX`.

## Validation and next decision

The default CPU/bytecode suites passed 81 tests; the combined experimental
features passed 89. Region tests compare separate traces, ordinary regions and
the specialization control flow across budgets, calls, returns, stack faults
and dirty code. Direct-link tests cover 32/64-bit constants and a branch that
bypasses the constant load. The five native artifact/profile tests passed.

`shared-exit,virtual-flags` passed the existing **560** specialized Wasm state
comparisons against `faf6988`: registers, flags, RIP, retirement, memory, faults
and dirty code. The 100,000-iteration run was a correctness smoke check; its
Docker/Wasmtime timings are not browser performance claims. Both new paths were
checked separately in browser continuations; their joint combination was checked
by native region tests. There is no exhaustive compiled-region hardware oracle.

Every captured continuation passed the existing interpreter comparison; every
warmed trial checked its expected final state. The harness also rejected
deliberately corrupted hashes, output, retirement, process state and region
counts. Modified JavaScript/shell syntax and diff whitespace checks passed.
No CI jobs, host imports or automatic Assembly activation were added.

The immediate architectural opportunity is reusable compiled artifacts and
amortized activation. A code-quality investigation should profile **warmed**
code and retain the static/dynamic distinction: eliminating thousands of cold
stores was less useful here than allowing the existing generated code to warm
up. General OCI coverage, phase changes, live-resource handoff, parallel
compilation and end-to-end amortization remain open.
