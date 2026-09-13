# Exploring a 2× execution speedup

This branch now contains a working partial-evaluation prototype. It does **not**
yet provide a general 2× container speedup. The container baseline is `faf6988`,
including the register-array optimization; that earlier gain is not counted again.

## Implemented experiment

Run `./tools/microbench/specialize.sh` from the repository root. It uses local
amd64 Docker, downloads checksum-pinned weval 0.5.0, builds a Wasm test module,
specializes its actual Zaqaru bytecode interpreter, checks state, and measures
five alternating baseline/specialized pairs per fixture. Nothing runs in CI.
The default is ten million loop iterations. Results, module hashes, environment,
and specialization time are written beneath `benchmark-results/`.

The `specialize` CPU feature enables an experimental entry point. The normal
engine uses the same shared interpreter with specialization disabled. Register
reads and writes become weval virtual-register intrinsics only in the experiment;
context annotations retain the bytecode PC across control-flow merges. The normal
build has no weval imports or dependency. The experiment's ABI follows
[weval's header](https://github.com/bytecodealliance/weval/blob/v0.5.0/include/weval.h).

This does not translate a second implementation of the x86 operations. It
specializes the current engine with constant bytecode and instruction-address
buffers, leaving registers, flags, memory permissions, and the instruction budget
dynamic. The fixtures are x86 instruction sequences decoded and lowered by the
existing decoder and transpiler. There are no language or application checks.

Validation compares all architectural GPRs, status flags, RIP, retirement, guest
memory, fault address/access, deferral offset, and dirty-code pages. The 560 cases
vary loop length, incoming flags, seven budgets, read-only pages, out-of-range
accesses, cross-page writes, and writes to marked code pages. Timed runs assert
that the loop counter reaches zero and the exact expected work retires. Any
remaining reachable weval intrinsic traps rather than quietly using a slow stub.
The existing 51 CPU unit tests and 27 bytecode differential tests also pass.

The final ten-million-iteration run on local amd64 Docker (core 0) compared
against the actual `faf6988` CPU and x87 sources with identical fixture setup.
The script extracts those sources into an ignored directory, prunes the workspace
lockfile offline, and verifies that dependency versions/checksums do not change.
Only its unused specialization entry point is replaced by a trap; the reference
bytecode interpreter, decoder, and transpiler are unchanged.

| Fixture | Median paired speedup versus `faf6988` |
| --- | ---: |
| Arithmetic/rotate | 7.77× |
| Permission-checked indexed loads/stores | 3.76× |
| Flags, carry, conditional moves, partial-width writes | 5.88× |
| Multiply/shift | 8.23× |

These are engine fixtures, not OCI applications. They are not aggregated into a
"general" score. The four-function specialization took approximately one second
(rounded to whole seconds), outside the timings. The emitted specialization
contained 1,031 instructions across four functions; the generic function had
5,138 instructions according to weval's IR statistics.

An earlier arithmetic fixture stopped at an unsupported rotate before completing
its loop; its timing was rejected and the fixture corrected. The final harness
checks completed work explicitly to prevent that false result recurring.

The normal-build scaffold was also compared with the saved `faf6988` executable.
A trace abstraction regressed ordinary ALU and random-memory execution by
13–15%, so it was removed. The final implementation retains the original normal
code reference and bulk register copies. Its six-workload, three-repeat check
measured throughput ratios of 1.018× mixed, 1.003× calls, 0.957× ALU, 0.941×
random memory, 0.899× syscalls and 0.977× floating point (0.965× geometric mean).
The short syscall subtraction is noisy; earlier larger syscall experiments also
showed small regressions. These residual costs are an unresolved part of the
experiment, not represented as zero overhead. The branch is a research checkpoint,
not a production acceleration tier; normal execution does not use weval yet.

Compilation/specialization and Wasmtime module instantiation are excluded from
these loop timings; state setup/serialization are included on both sides. The
script records specialization time separately. These fixtures have no inter-trace
calls, dynamic code discovery, or application-level workload mix, and the checks
do not establish correctness of every supported x86 instruction under weval.

## Remaining architecture decision

The prototype clears the hot-loop experiment's speed threshold. Integration is
still required: automatic discovery, multi-trace control flow, invalidation,
coverage measurement, and end-to-end comparisons against `faf6988`.

For code discovered after bake time, the proposed next stage is an **optional**
host-assisted compiler. The guest would choose hot traces and produce a request;
the host would install sandboxed Wasm sharing the instance's memory and table.
Cold or uncompiled code would continue through the existing engine. Compilation
cost and cache misses must be included in application results, and scheduling
must stay instruction-count based. Installed code must be reconstructed alongside
snapshots, and every entry must be invalidated when its source code changes.

This changes the host's explicit two-import contract (`crates/host/src/lib.rs`),
so it is a user decision, not something the experiment silently introduces.
Keeping that contract instead would focus the next stage on automatic bake-time
specialization, with unaccelerated runtime-generated code as a measured limitation.

## Scope: arbitrary x86 / OCI containers

The optimization must generalize across x86 programs and OCI images. It must
not require a particular language, library version, application, or rebuilt
guest binary. Language-runtime replacement, Python-specific fast paths, and
application-specific semantic shortcuts are out of scope. Python and other
runtimes may be validation workloads, not prerequisites for acceleration.

Specialization may depend on actual instruction bytes and observed execution,
provided discovery and optimization are automatic and use x86 semantics alone.
Unknown code must remain executable through the interpreter, and runtime code
generation, dynamic loading, and self-modification must retain their behavior.
An interpreter fallback preserves compatibility; it does not demonstrate that
the speedup generalizes. Coverage and gains must be measured on diverse images,
including code that was unavailable at bake time. A general mechanism does not
promise an identical 2× gain for every workload.

## What the archive rules out

The old compiler experiment went further than compiling individual blocks:
it also put roughly 4,000 libpython blocks in one Wasm region, retained machine
state in locals across internal transfers, replaced its resolver with a direct
cache, and tried wasm-opt. It still lost on CPython. Merely proposing larger
regions repeats an experiment already tried. See
[the tier-1 record](archive/tier1-plan.md), particularly its final outcomes.

That result describes the old implementation and its per-instruction costs;
it is not a proof that every possible compiler must lose. A new experiment
needs a specific reason its emitted work is cheaper.

## Primary experiment: specialize the current bytecode engine

Treat a known bytecode trace as constant input to the current interpreter and
produce a specialized Wasm function at bake time. Constant opcodes, operand
indices, widths, and branch targets can eliminate dispatch and operand decoding.
Promote guest registers and lazy flags into compiler-visible values, using
explicit state synchronization at exits. Retain the current memory permission
and code-invalidation semantics. Start with the present fast implementation
rather than revive the old compiler's helper-heavy generated code.

The relevant precedent is [weval](https://github.com/bytecodealliance/weval),
which specializes Wasm interpreters using known bytecode. Its project reports
3–5× gains for its SpiderMonkey use case; those are not Zaqaru predictions.
The [PLDI paper](https://cfallin.org/pubs/pldi2025_weval.pdf) explains why
specializing dispatch alone is insufficient and provides state intrinsics
that turn virtual-register accesses into compiler values. Integration into
Zaqaru would require adaptation, not just running a tool on the current module.

A bake-time specialization pass can preserve the single-module/two-import
contract. Known code gets specialized; unknown code remains interpreted.
This makes bake-time specialization a compatibility-preserving candidate, but
its performance on runtime-generated code remains an explicit limitation. A
fixed corpus of known application or runtime functions is not the solution.
Relocations, executable-page identity, and writes to code must invalidate or
prevent entry to incompatible specialized code. Profiling chooses what to
specialize; it must not be treated as a correctness proof about future inputs.

First establish a small upper-bound experiment:

1. Save the current engine as the new baseline.
2. Specialize complete loops from mixed, calls, and an application-runtime
   workload, including memory checks, exact faults, retirement and exits.
3. Inspect whether generated code actually removes opcode dispatch, dynamic
   register indexing, and unnecessary flags work. Measure separately from
   compilation and specialization time.
4. Require at least 3× speedup in a substantial hot region before expanding
   coverage. This is an experimental acceptance target, not a forecast.
5. Measure wall-time coverage and exit costs on a representative application.
   A 3× faster tier needs to cover 75% of execution time to yield 2× overall.

The application-runtime experiment first needs a working guest fixture: the
previous minimal Python rootfs attempts stopped at unsupported syscall 99.
A larger implementation should wait for evidence from the small experiment.

## Other bets

| Direction | Cost removed | Principal constraint |
| --- | --- | --- |
| Generate fused bytecode handlers from hot instruction sequences | Multiple dispatches, temporary-register traffic, redundant width/flag work | Code size and coverage; still an interpreter, so 2× is less certain |
| Host-assisted runtime compilation to Wasm | Interpretation of hot code discovered only during execution | Extends the host contract; needs code installation, invalidation, and snapshot reconstruction |
| Restore a prebooted snapshot | Repeated initialization | Startup improvement, not steady execution throughput |
| Replicate independent containers across cores | Single-instance aggregate capacity limit | Throughput scaling, not faster execution of one request; external state must support replication |

For runtime compilation, generate sandboxed Wasm and ask the host to load it;
standard Wasm code cannot install new executable functions by itself. Exact
preemption boundaries, guest instruction accounting, fault state, and code
invalidation must agree across tiers so compilation availability cannot change
the recorded schedule. Code caches would need to be rebuilt or restored beside
memory snapshots. The author's [weval discussion](https://cfallin.org/blog/2024/08/28/weval/)
also describes this hostcall approach and its operational tradeoffs.

Among these options, runtime compilation offers the most direct route to
discovering and accelerating arbitrary hot x86 code, including code created
after bake time. If the host contract must remain unchanged, prioritize
generic bytecode fusion and bake-time specialization, while measuring the
latter's coverage limits explicitly. Changing the host contract is a design
option under discussion, not an approved requirement change.

## What counts as 2×

Use the same workload, fidelity, and execution mode on both sides. Keep startup,
steady compute, and HTTP throughput separate. Report per-workload results and
regressions alongside any aggregate; do not count the earlier 14.9% gain again.

For a fraction p of wall time accelerated by s, overall speedup is
`1 / ((1 - p) + p / s)`. Thus 75% at 3× yields 2×; doubling a small hot loop
does not double the application. The historic Django split was about 11 ms
of interpretation and 9 ms elsewhere. At that split, a 2× engine alone yields
only about 1.38× request speedup; host/network overhead must improve as well.
Those old fractions need remeasurement before they guide an HTTP target.
