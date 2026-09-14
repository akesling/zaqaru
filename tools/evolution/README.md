# Runtime specialization experiment

The stacked [multi-trace weval experiment](../../docs/weval-ten-times.md) adds
an opt-in `regions` build and per-region coverage counters. It records both
incremental comparisons and regressions; a general 10× gain is not established.

This experiment discovers x86 bytecode traces while a real baked container runs,
freezes its continuation, and produces a successor executor containing specialized
Wasm and the interpreter fallback. The optimizer itself is a Wasm Block using
the existing Featherweight core binding. The same demo runs in Node and a Chrome
Worker; neither invokes a native compiler or a compilation server at runtime.

The browser path is preferred, but AOT/server-side optimization remains in scope.
Docker supplies build tooling and x86 fixture binaries. Nothing is added to CI or
to the normal workspace's dependencies. General binary ALU operations on high-byte
registers are also lowered into bytecode, independently of the experimental tier.

## Run locally

From this checkout, with Docker, Node 25, and a local StructFS checkout containing
commit `81b37c51e9e400c214571b40632c8f675448f3f9`:

```sh
bash tools/evolution/run.sh prepare /path/to/structfs
bash tools/evolution/run.sh build mixed 4000000
bash tools/evolution/run.sh node
bash tools/evolution/run.sh browser
```

The browser test uses installed Google Chrome on macOS. Set `CHROME` to another
Chrome/Chromium executable. It starts a loopback static-file server and headless
Chrome, runs the actual Worker, then closes both. The page can also be served as
`tools/evolution/index.html` using a static server rooted at the repository.

`prepare` archives the pinned StructFS and historical Zaqaru sources into ignored
`benchmark-results/`, checks out pinned weval 0.5.0, and builds the existing browser
host's TypeScript. The evaluator is adapted into a generated, single-threaded
library: no Wizer/Wasmtime initialization, SQLite cache, Rayon execution, clocks,
or filesystem diagnostics. Its original license is retained beside those sources.
`compiler-Cargo.lock` pins this separate compiler build.

`build` uses the existing Docker wrapper. It builds the opt-in `evolution` guest,
the optimizer Block, a static x86 benchmark, and a `faf6988` baseline module with
identical arguments. Existing verified historical binaries can be reused; otherwise
the baseline is built from the archived revision. The native helper only bakes
fixtures and normalizes artifacts; checkpointing and compilation run inside Wasm.

## What is implemented

* Runtime trace discovery with bounded candidates, retirement-weighted selection,
  and a cache of validated bindings. Each decoded trace has a unique identity;
  newly decoded code is compared before reusing a specialization at its address.
* A quiescent `freeze(token)` / `retire(token)` / `resume(token)` guest state machine.
  Frozen and retired instances refuse execution. A retired instance cannot resume,
  and a resume token is consumed. Tokens are controller bookkeeping, not capabilities.
* Snapshot data and the stack pointer become the initial state of a generated
  module. Unsupported state models are rejected. Function table slot identities
  remain stable; compiler-only imports become local traps. Runtime executors import
  exactly `env.ll_read` and `env.ll_write`.
* An optimizer Block with exactly `structfs.read` and `structfs.write`. Writing
  `executable` or `compile` creates a `results/N` value; reading it returns Wasm
  bytes as base64. It uses the existing server protocol and browser binding.
* Browser/Node checks for output, final process state, retirement counts, actual
  compiled execution, frozen execution refusal, wrong tokens, repeated activation,
  and retirement. Artifact tests check import remapping, duplicate import rejection,
  memory replacement, and unsupported globals.

## Measurement limits

Results are written to `benchmark-results/evolution-{node,browser}-KERNEL.json`.
They are single-run exploration measurements, not a statistically established
general speedup. `normalizedBaselineSpeedup` compares the execution tail against
`faf6988`, adjusting for at most one scheduler quantum of warmup drift.
`executionSpeedup` instead compares the exact same continuation against the
experimental interpreter, which has profiling overhead and is not the performance
baseline. `transitionMs` includes freezing, snapshotting, optimization, and successor
instantiation; `baselineIncludingTransition` includes that cost. Warmup and initial
startup are outside both tail measurements.

On the local Chrome test, the mixed arithmetic/memory workload's execution tail
improved about 6.9× against the historical baseline after high-byte ALU lowering.
Its transition took about 2.2 seconds, so this short workload was slower overall
when transition cost was included. Earlier ALU tests demonstrated compiled execution
in Chrome but used the experimental interpreter comparison; do not treat those
numbers as historical-baseline results. These observations do not establish the
requested general 2× speedup.

A longer Chrome run (`mixed 100000000`) did amortize the transition: 9.75 s for
the historical baseline tail versus 0.81 s of successor execution plus 1.65 s
for the transition, about **4.0× including transition**. Output and retired work
matched, subject to the recorded warmup quantum difference. This is one long
workload and one sample, not evidence that arbitrary containers improve 2×.
The calls workload (`calls 40`) measured only about **1.27×** against `faf6988`
in Node, despite nearly complete compiled coverage, and lost when transition cost
was included. Frequent transfers between traces remain a limiting case.

Keeping cached transfers inside the bytecode runner improved the calls experiment
further. Three interleaved Node samples per variant gave median historical-baseline
tail speedups of **1.31× before and 1.44× after** (median successor times 92.3 ms
and 84.4 ms). Two Chrome checks measured 1.18–1.33× after the change. All runs matched
output and retired work; the short workload still loses when compilation is
included. This loop returns to the engine for dirty code, cache misses, faults,
interpreter fallback, and exhausted budgets. It still spills machine state between
compiled traces, so it does not solve the call-heavy 2× target.

The CPU unit and bytecode suites can run in local Docker. The separate hardware
lockstep suite currently fails at `PTRACE_GETREGS` under local amd64 emulation;
it cannot provide hardware-oracle validation in this setup.

## Still required for Assembly evolution

The demo directly drives source and successor instances. It is **not yet an
Assembly-management implementation**. The architectural target remains an optimizer
Block producing an immutable artifact and a controller creating a successor Assembly
through StructFS management, with a stable public interface and explicit ownership
handoff. It must not become a compiler store that silently replaces a running module.

Before enabling automatic transitions, implement artifact/engine lineage validation,
exclusive activation authority, failure recovery, and external-resource ownership.
The demo uses snapshot-capable local mounts; it does not transfer live network
connections. It currently performs one optimization generation, freezes during
compilation, chooses at most 32 traces, and uses substantial memory (the fixture's
guest memory alone is about 595 MiB). Phase changes, low-coverage workloads, compilation
amortization, and broader correctness/performance testing remain open. Browser
execution is demonstrated; mobile-browser resource suitability is not.
