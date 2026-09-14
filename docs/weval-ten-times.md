# Weval: beyond single-trace speedups

Branch: `perf/weval-ten-times`, stacked on PR #3 at `adcc9a0`.
The target is a broadly useful 10× improvement with general x86 semantics,
prioritizing browser/Wasm execution. It has **not** been achieved. Historical
ratios still use `faf6988`; incremental A/B comparisons use the saved PR #3
single-trace artifact. The earlier mixed-loop gains are not new gains here.

## Multi-trace experiment

The opt-in `regions` feature combines observed bytecode traces into one immutable
stream and retains virtual registers across internal transfers. Branch offsets
are relocated while fault IPs retain their guest addresses. A constant balanced
tree maps dynamic guest targets to member offsets. Unknown targets, dirty code,
faults and interpreter fallback leave the region; instruction budgets are checked
at the original transfer boundaries.

One hot root becomes a region with 2–32 selected members (default 8). Other roots
retain single-trace compilation. Every member must match its frozen identity in
the current code cache before entry. A missing or changed member refuses region
entry; this conservative check can lose coverage after eviction. It does not
substitute addresses for code identity or assume anything about the guest language.

The compiler still runs as Wasm through the two existing store imports. No
Assembly controller, automatic recovery or new host imports are added here.
New exports configure and measure the manual experiment only.

## Results so far: `calls 40`

Successful runs produced `calls 1854720` and retired 42,570,617 instructions,
with 10,010,212 before freeze. These short tails do not amortize the roughly
2–4 second transition. Individual samples are exploratory, not stable estimates.

| Region experiment | Region retired / entries | Observed result |
| --- | --- | --- |
| Linear lookup, 8 members | 15,471,925 / 210,220 | Node 101.8 ms; 1.15× historical baseline |
| Linear lookup, 16 members | 20,383,634 / 66,048 | Node 137.4 ms; 0.84× historical baseline |
| Balanced tree, 22 members | 32,514,612 / 694 | Three alternating Node pairs: 0.976×, 1.210×, 1.065× relative to PR #3; median 1.065× |
| Balanced tree, 8 members | 15,471,925 / 210,220 | Chrome 87.1 ms; 1.23× historical baseline |
| Balanced tree, 22 members | 32,514,612 / 694 | Chrome 139 ms; 0.79× historical baseline |

The 22-member region handles 99.9% of compiled retired instructions and averages
46,851 instructions per entry, versus 74 for eight members. Reducing exits did
not yield a large gain and regressed the Chrome sample. Next, examine the work
remaining inside generated code and browser compilation behavior.

The linear 32-member attempt exceeded weval's evaluation limit. Early balanced
lookup attempts let context indices become runtime values and trapped during
evaluation. The working tree precomputes child indices and keeps transition
contexts distinct from destination contexts, including the missing-target case.
Compiler panic messages and evaluation-limit warnings now pass through StructFS
diagnostics; failures are rejected rather than accepted as measurements.

## Exposing the permission-cache hit

The next change separates `Space::permitted` into its cached-page fast path and
the full permission walk. In region builds the fast path is forced inline, so
weval sees the fixed access width and permission kind. The slow path stays out
of line to avoid duplicating the page walk in every compiled memory operation.
Ordinary builds inline the helper back into the existing permission function.
The permission cache, overflow checks, fault addresses and invalidation rules
are unchanged.

With both templates configured for 22 members, three alternating Node pairs
measured **1.231×, 1.200× and 1.009×**, median **1.200×**, relative to the region
checkpoint. Median successor time went from 77.2 to 64.3 ms. A Chrome sample
went from 139 to 88.7 ms, but those browser samples were not interleaved.
The new Chrome result is only 1.21× against the historical baseline, and its
2.68-second transition still overwhelms the short execution tail. This is an
incremental improvement, not a 10× result or a general-container claim.

The paired samples are in `benchmark-results/region-permissions-comparison.json`.
Both ordinary (81 tests) and region (83 tests) CPU/bytecode suites passed.
The browser continuation checks passed with both 8- and 22-member regions;
the permission-cache candidate was checked with 22 members.
A separate `mixed 4000000` Node check also passed output, retirement and
continuation validation with the candidate. It measured 44.8 ms of successor
execution, 354.2 ms of historical baseline and 2.28 s for transition (7.92×
normalized tail speedup, 0.15× including transition). This is a single sample,
not an incremental comparison against PR #3 or a new general 10× result.

## Reproduction and validation

After preparation described in `tools/evolution/README.md`:

```sh
bash tools/evolution/run.sh build calls 40 regions
bash tools/evolution/run.sh node 22
bash tools/evolution/run.sh browser 22
./tools/microbench/experiment.sh test --features regions
```

The third build argument defaults to `evolution` (single-trace). Save each baked
template before another build replaces it. Interleave two saved templates with:

```sh
node tools/evolution/compare-node.mjs BEFORE.wasm AFTER.wasm OUTPUT.json 3 22
# Region versus region: configure both sides explicitly.
node tools/evolution/compare-node.mjs BEFORE.wasm AFTER.wasm OUTPUT.json 3 22 22
```

The last argument configures the candidate region size. Each run validates its
continuation against an experimental reference and historical completed work;
the pair checks matching output and total retirement. JSON retains hashes, raw
timings, transition costs, region coverage and normalized paired tail ratios.
`benchmark-results/region22-comparison.json` holds the local comparison.

CPU tests compare separate traces and regions across calls, returns, budget
boundaries, stack faults, dirty-code stores and targets outside the region.
These and browser continuation checks do not establish correctness for every
x86 region. Existing Assembly evolution and native x86/Docker hardware-oracle
limitations still apply. No CI is added.
