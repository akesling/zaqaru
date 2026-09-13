# Proposal: a Wasm compilation store

**Superseded draft.** The activation API below puts executor-specific machinery
behind store paths rather than expressing the operation as Isotope composition.
The revised direction is a Block producing Wasm artifacts and an authorized
controller composing and activating a successor Assembly version through existing
assembly management. Assembly definitions remain immutable; state/continuation
handoff between executor Blocks needs an explicit protocol. Compiled regions
should remain within an executor Block unless a sufficiently coarse inter-Block
interface can amortize communication. This draft is retained for context, not
as the recommended architecture.

Status: proposal only. No implementation or new import is authorized by this
document. The container must continue to import exactly `env.ll_read` and
`env.ll_write`. This follows the capability model described in
`crates/kernel/src/abi.rs`: adding a capability means adding a mount.

## Capability and protocol

Mount an optional compiler store at `/iso/compile`. The namespace below is a
proposal for Zaqaru's StructFS boundary, not an existing StructFS standard.
Initially the backend runs locally alongside Wasmtime in Docker.

| Operation | Meaning |
| --- | --- |
| Read `capabilities` | Protocol version, supported specialization/engine ABIs, and limits |
| Write `jobs/{id}/request` | Submit an immutable specialization request |
| Read `jobs/{id}/status` | Pending, ready, or failed, with a diagnostic on failure |
| Read `jobs/{id}/result` | Content-addressed artifact reference and compatibility metadata |
| Read `artifacts/{hash}/module` | Retrieve the generated portable Wasm module |
| Write `activations/{id}/request` | Request activation of a compatible artifact in this instance |
| Read `activations/{id}/result` | Pending, applied, or rejected, with the artifact/binding identity |

All paths are relative to the mount. Requests and responses are versioned bytes;
metadata can use JSON and modules remain binary. The guest chooses deterministic
request IDs, so it knows where to read results even though the current kernel
Store trait discards the result path returned by a write. Retrying an identical
request is idempotent; reusing its ID for different contents is an error.

The specialization request identifies the engine's Wasm template, its ABI and
memory layout, the bytecode/IP buffers to specialize, and the originating x86
code identities. Cache identity includes all semantic inputs and compiler
configuration. It must not depend only on an x86 address or an application name.
No live TCB, application data, or full process-memory snapshot needs to be sent
to a remote service; the initial backend is local in any event.

Zaqaru still discovers and lowers x86 instructions. The store specializes Wasm
with supplied constant inputs; it does not grow a second Linux personality or
language-specific execution engine. An absent mount or rejected compilation
leaves execution on the existing interpreter.

## Activation without additional imports

Returning Wasm bytes does not make those bytes executable. The runtime adapter
must perform activation. The ordinary host Store trait has no Wasmtime Caller
or function-table access, so this is real host implementation work, not merely
another in-memory store.

For the first implementation, prefer **replacement of the module between
execution turns**, rather than dynamically linked helper modules:

1. The store queues an activation request and returns normally. It never
   re-enters the running guest to switch its code.
2. At a defined turn boundary, the host validates a candidate module containing
   the original engine plus the specialized functions. Its only imports must
   still be `ll_read` and `ll_write`; compilation-only weval intrinsics must not
   survive as runtime imports.
3. The host instantiates that module and migrates the *current* machine state,
   not a stale state captured when compilation was requested. The artifact must
   preserve the existing memory layout, mutable state, exports, and existing
   function-table identities. Compiler-owned data needs an explicit compatible
   layout; blindly overwriting a new module's memory is insufficient.
4. An activation hook exported by the guest binds the new functions to matching
   code identities. It must reject code that was modified or unmapped while
   compilation was pending. Exports do not add host imports.
5. Only after successful preparation does the host replace the live instance
   and expose the applied activation result. Failure leaves the original
   instance runnable.

This avoids both a third host-function import and shared-memory/table imports
for helper modules. It also compiles and reinstantiates a whole module, so it
may be expensive. It is a compatibility-first experiment whose installation
cost must be measured, not assumed away. Later installation mechanisms would
need their own proposal while retaining the two-import constraint.

The compiler backend can be reusable, but activation is instance-local. A plain
StructFS client can submit requests and inspect artifacts; an arbitrary store
cannot install executable functions without cooperation from the runtime.
Installation authority therefore belongs to the configured mount/runtime
adapter, not to an unverified integer returned by a remote compiler.

## State, invalidation, and replay

Bindings use logical artifact/code identities rather than portable claims about
native pointers or Wasmtime handles. Snapshot state must include the active
module identity, binding metadata, and the store's pending/applied requests.
Restore reconstructs the corresponding code as well as linear memory. The
existing memory-only snapshot representation is not sufficient for this tier.

Compilation completion can vary in wall time. Poll at deterministic guest
execution points and record the store answers that make a tier available.
Replay uses the recorded answers and retained artifacts, activating at the same
logical boundary. A tape alone is not enough if its compiled artifacts are gone.
Tier changes must preserve the baseline's quantum boundaries, instruction
accounting, faults, and code-write invalidation. Faster execution must not change
the guest schedule or observable I/O ordering.

Already-compiled code must be invalidated on source writes, unmaps, and remaps;
validating only at installation is insufficient. All of this applies to code
generated after bake time, regardless of the guest language.

## First acceptance experiment

Implement the store and activation adapter behind an explicit local experiment
option. Automatically discover one hot loop from actual execution, compile it,
activate it, and demonstrate correct continuation. Then exercise pending code
mutation, compilation failure, snapshot/restore, and recorded replay. Verify
the final module's import list directly.

Measure compilation, activation, cache hits, and total execution against
`faf6988`, alongside steady execution. Extend to multiple traces and diverse OCI
workloads before claiming a general 2× result. The existing specialization
prototype's 3.76–8.24× loop gains do not establish that installation amortizes or
that enough application time is covered. Fix the scaffold's normal-path
regressions as part of integration. Add no CI jobs or Python-specific paths.
