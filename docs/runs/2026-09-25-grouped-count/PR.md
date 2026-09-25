# perf: add opt-in generation-protected grouped exact COUNT

## Status

Draft PR description; **no PR has been opened from the authoring environment**.
Base: `737a2713302075243d45f37ad49e11fa57c85dbc`.
Branch: `perf/grouped-count-page-popcount`.
Native compilation/qualification and independent FFI/visibility review remain
blocking. `pin.enable_grouped_count` defaults off. No new performance result or
10x/TIN-parity claim is made.

## Implemented

Expose `ExactSink`/`scan_exact` from the existing grouped evaluator. Bitmap
consumers keep their scalar adapter and correct canonical fallback. COUNT consumes
sealed heap-page offset masks without expanding them into TIDBitmap. Sparse and
frontier membership stays scalar and owner-qualified.

Extend the existing PinCount planner/executor path to supported exact Boolean
queries. Acquire structural share, then nonblocking writer-interlock share before
any source read; keep generation protection through all visibility decisions.
Fresh VM certification enables page popcount. Dirty roots use the PostgreSQL
HOT/MVCC fetch API without detoasting/re-tokenizing an already exact predicate.
Runtime contention, unsupported snapshot/source/budget or disabled cached-plan
eligibility uses the retained PostgreSQL aggregate. Errors do not publish a
partial count. Phrase/prefix, row-returning and ranked queries remain ordinary
executor paths. Add work counters and generation/visibility fault-injection tests.

No disk format, analyzer, WAL, dependency or SQL migration change. Rebuild all C
and Rust objects together and restart preload for the private counter ABI change.

## Why this slice

The older archived broad-query profiles contain roughly 74-80% PostgreSQL self
samples. A faster word intersection alone cannot remove that whole cost. The new
consumer removes identifiable bitmap construction, tuple production and aggregate
work, and permits heap avoidance only after source/visibility certification.
It does not claim that sampled DSO shares are exact phase timings.

The alternatives record compares primary compact postings, scalar/SIMD kernels,
bounded mutable segment sealing, exact counts, same-score top-k and copy/WAL
reductions. Bounded durable sealing/merging and score-aware top-k are not claimed
implemented. Historical PR21's final `rare` label actually returned 8,212 of
25,576 rows; the new harness maintains a genuinely rare term separately.

## Performance by query class

| Class | New measured improvement | Intended change / remaining limit |
| --- | --- | --- |
| Rare | Unmeasured | Sparse canonical path retained; guard/setup may regress tiny queries |
| Selective AND | Unmeasured | Exact grouped membership and visibility shortcut where certified |
| Broad AND/OR and exact COUNT | Unmeasured | Page popcount removes per-match bitmap/heap/aggregate work on certified pages |
| Dirty-page count / long delta | Unmeasured | HOT/MVCC still required; owner-frontier parsing and lock duration remain |
| Phrase and returned rows | Unmeasured | Ordinary semantic fallback retained, no special speedup claimed |
| Ranked top-k | Unmeasured | Same PostgreSQL `ts_rank_cd` controls; exhaustive ranking remains |
| Writes, WAL, size, build time | Unmeasured | No persistent storage rewrite; subsequent writers can wait behind the shared guard |

The last measured PR21 controls remain the historical evidence, not results from
this PR. The paired harness records old PIN/new PIN/GIN in six balanced orders,
full identity and rank equality, per-sample CPU/latency/throughput, buffers and work
counters, build and one-index write/VACUUM CPU/WAL/space, raw plans/logs and hashes.
It defaults to 20k and 1m varied synthetic rows. Large natural-language and
sustained concurrent writer workloads still need execution and qualification.

## Correctness evidence

Actual local checks: 144 Python methods pass, including 21 new COUNT-focused
methods. The actual new C bridge bodies pass debug and UBSan/bounds host-double
runs; 13 rejected cases are checked. A finite model explores 246 states/339
transitions for six cases and finds witnesses for missing generation protection,
copy-before-protection, cached VM and partial output. Existing false-AND generation
counterexample remains a negative control. These do not prove native PostgreSQL.

Added but unrun: pure Rust mask/identity/oracle tests; native normal/test-hook
fresh and short/long deltas, rare/common/AND/OR/NOT/phrase, NULL/empty, HOT,
UPDATE/DELETE, own writes, asserted CTID reuse, VM on/off, cached plan, low memory,
RLS, concurrent readers/writers, old snapshots through VACUUM, contention fallback,
ERROR/cancel/terminate before and after partial visibility work, immediate
restart/recovery with a durable witness. Existing standby-read refusal is retained.
G0 now schedules these native runs and accepts `perf/**` push branches.

## Tradeoffs and gates before approval

The generation guard is intentionally coarse. Nonblocking acquisition avoids
waiting for an active writer, but a successful count can block later writers and
VACUUM for its duration. Structural acquisition can still wait for maintenance.
Do not enable production use without measured writer p95/p99/throughput and an
independent review of VM, HOT, retirement and error-unwind obligations.

Native Rust/PG compile, rustfmt/Clippy, full memory failure/RSS, crash/replay,
hardware/large-corpus paired results, sustained maintenance debt and direct TIN
comparison remain unresolved. There is no evidence for declaring the 10x target
achieved or this extension production-ready.

Detailed paths/proof/alternatives: `docs/g9-grouped-count.md`.
Immutable contracts: `docs/api-evidence.md`, COUNT03.
Observed tests, failures and historical reanalysis:
`docs/runs/2026-09-25-grouped-count/README.md`.
