# Grouped exact COUNT: a protected page-mask consumer

Status: implemented behind `pin.enable_grouped_count = off`. The implementation
has not been compiled with Rust/PostgreSQL or benchmarked natively in the authoring
environment. Local C host-double tests and finite protocol models are not a proof
of PostgreSQL concurrency, ABI, recovery or performance. Independent review and the
native qualification matrix are blocking gates. No query class has a new measured
10x result, and no matched TIN run exists.

Base: remote main `737a2713302075243d45f37ad49e11fa57c85dbc`, including PR21 and
PR22's qualified measurements. The uploaded ZIP matched that exact commit/tree.
The focused implementation changes the *consumer* of existing grouped postings,
not their persistent encoding. [API evidence, COUNT03](api-evidence.md#count03-grouped-exact-count-generation-interlock).

## Evidence before choosing the slice

The reproducible [historical reanalysis](runs/2026-09-25-grouped-count/historical-reanalysis.json)
reads the committed compressed raw records, preserves archive/member hashes, and
extracts DSO and flat self-sample reports. It is not a new native experiment.

PR21 same-backend, 4,096-related-write medians, microseconds of backend CPU:

| Historical query label | Owner off | Owner on | GIN | GIN / owner-on |
| --- | ---: | ---: | ---: | ---: |
| rare | 3,820.2 | 3,735.7 | 2,759.8 | 0.739x |
| selective_and | 4,249.8 | 2,577.4 | 2,678.3 | 1.039x |
| and | 6,221.8 | 5,075.5 | 7,244.2 | 1.427x |
| or | 5,791.5 | 4,611.0 | 4,916.5 | 1.066x |

Important distribution qualification: both `rare` and `selective_and` returned
8,212 of 25,576 rows after related writes. The historical labels are retained, but
these particular samples do not establish genuinely rare final selectivity.
The new corpus keeps `rareplanet` out of all added deltas.

The older G9 fresh-index profiles contain 73.63% PostgreSQL self samples for broad
AND, 80.20% for common-term queries, and 77.68% for OR. Rare and selective AND have
35.01% and 38.77% libc self samples respectively. The flat reports attribute large
shares there to `memmove`; broad reports contain heap/HOT, executor, bitmap and
aggregate work. Write profiles contain substantial page validation and copying.
These are sampled, small, warm VM fixtures from an earlier revision. They do not
supply exact PR21 CPU times for every phase, and libc samples must not all be
assigned to PIN page copies without call-chain attribution.

The archive cannot cleanly separate index traversal, private-page validation,
owner payload parsing, TIDBitmap construction, heap visibility, expression
rechecks and maintenance into additive CPU intervals. This patch adds work
counters, not fabricated phase timers. Exact phase timings, allocator/RSS and PMU
measurements remain required on the new binary.

### Limits of a bitmap-only speedup

A conditional Amdahl illustration: if all 73.63% of the broad-AND PostgreSQL self
fraction were invariant, eliminating every other instruction would cap that
particular execution at `1 / 0.7363 = 1.358x` relative to itself. This is not a
hardware lower bound: some PostgreSQL work, notably bitmap and aggregate work,
is removable by a different executor path. It is why another offset kernel alone
is not evidence for a broad-query 10x claim.

For returned rows, projection, visibility and delivery still require work
proportional to the number of returned rows. For a dirty-page exact count,
visibility work remains proportional to candidate roots on those pages. On
certified pages, the new path replaces per-root bitmap/heap/aggregate processing
with one VM probe and eight scalar word population counts per result page, plus
index traversal and frontier work. For ranked top-k, without safe score bounds,
all potential candidates may still require visibility, scoring and comparison.
None of these formulas establishes an absolute microsecond floor without native
measurements of the relevant executor and data distribution.

## Alternatives compared

| Option | Work it can remove | Decision for this slice |
| --- | --- | --- |
| Make compact page/offset postings primary | Canonical-owner lookup, bytes copied, term parsing; early AND page pruning | Reuse the existing complete grouped snapshot and sparse path. Replacing authoritative storage also needs write, retirement, migration and rebuild proofs; not claimed implemented here. |
| CPU-dispatched bitmap kernels | Some word operations | Retain existing scalar/capability-safe kernels. Profiles do not justify a new unsafe SIMD boundary. Dispatch cannot remove heap or executor work. |
| Bounded mutable segments with incremental seal/merge | Repeated parsing of long owner frontiers; full VACUUM rebuild dependence | Valuable next architecture. Requires complete-document segment identities, atomic cutoff handoff, retirement coverage and backpressure. Existing frontier remains; no claim that this work has been solved. |
| Exact grouped COUNT CustomScan | TIDBitmap, per-result tuples, aggregate transitions, certified-page heap reads, and exact dirty-root text rechecks | **Selected.** Existing upper-path planner fallback and exact grouped evaluator allow a small vertical slice with a substantial amount of identifiable work removed. |
| Score-aware top-k CustomScan | Exhaustive ranking/sorting after score-bound pruning | Keep disabled/unimplemented. Existing PIN scoring is not automatically PostgreSQL `ts_rank_cd`. Need exact same-score upper bounds and visible-only threshold admission before comparing against that baseline. |
| Reduce page-copy/WAL validation | Write CPU, copied bytes and generic-WAL amplification | Measured hotspot, but changing buffer borrows/validation/WAL is a separate safety boundary. This read-only slice changes none of them. |

TIN's [public architecture](https://planetscale.com/blog/introducing-tin) motivates
page grouping, early intersection and bounded mutable work. It supplies neither
PIN's owner-generation proof nor permission to compare different ranking semantics.

## Current path trace

**Bitmap reads.** `am::bitmap` holds `storage::with_reader`'s structural ShareLock,
then runs grouped scanning or the canonical fallback. The grouped scan prunes
256-heap-page masks before needed offset payloads, evaluates term membership
against shared liveness, and processes the post-cutoff frontier. A single term
with at most 64 canonical postings retains the sparse scalar path. The bitmap
sink batches roots through `pin_bitmap_add` into PostgreSQL `tbm_add_tuples`.
PostgreSQL then performs bitmap heap/HOT visibility and required rechecks,
including its own lossy-bitmap rechecks. That path remains present and unchanged
in semantics; `ExactSink`'s default page method expands to its original scalar
callback.

**Page access.** `PgStore::read` uses the C buffer wrapper to pin/lock a PostgreSQL
page, copy its used payload, and release the content lock. The pure loader
validates the private copy. This patch does not return a shared page borrow or
remove validation. Its read counter includes repeated copies, not unique blocks.

**Writes.** Analyze/preparation is outside the normal writer interlock. Owner
reservation, fragments, dictionary/posting links and final publication remain
under the writer ExclusiveLock on index lock tag page 0. Queries ignore incomplete
publication. Owner coordinates and incarnations, not naked CTIDs, identify
mutable membership. Parallel index construction uses its existing build-specific
coordination and is excluded from this count path until the index is valid/ready.

**VACUUM and rebuild.** Canonical retirement clears grouped liveness independently
of activation GUCs before removing the owner and allowing heap reuse. Compaction
and grouped replacement take structural exclusion before writer exclusion; a
complete validated replacement is published through existing metadata. Old
journal storage cannot be recycled through an active structural reader. The new
consumer does not add a second retirement protocol.

**WAL.** Existing bounded generic-WAL page-copy modifications, buffer lock order,
publication and replay are unchanged. No durability setting, validation, flush or
heap work is disabled to obtain a performance comparison. The observed PR21 small
insert-plus-VACUUM penalty (about 2.1-2.2x CPU, 4.1x WAL and 2.5x index bytes versus
GIN) is not fixed by this patch.

## New execution and generation argument

`ExactSink` accepts sealed `(heap block, [u64; 8])` masks, sparse/frontier root
callbacks and sealed-query work statistics. `scan_exact` returns `None` *before
output* for unavailable format/syntax/budget. Errors invalidate all prior output.
It returns candidate cardinality, never a PostgreSQL-visible count by itself.

The host first acquires the existing structural reader barrier. It then tries a
**nonblocking ShareLock on writer tag 0**, before reading source metadata, owners,
terms or liveness. A queued/active conflicting writer causes ordinary-plan
fallback. Successful acquisition is held through the last VM/heap decision. No
upgrade or reader-to-maintenance transition occurs.

The correctness argument depends on the following audited host invariants and
must still be validated independently against native execution:

1. Normal inserts and all canonical retirement/publication are serialized by the
   writer interlock. While the shared guard is held, the captured sources cannot
   change owner incarnation or combine an old term-A owner with a reused term-B
   owner at one CTID. Sealed and frontier ranges are disjoint by the existing
   complete snapshot cutoff. One live owner per heap root remains a host invariant.
2. A writer that completed before acquisition published index contents only after
   the corresponding heap change cleared VM. The lock ordering plus ordinary
   shared-buffer reads preserve the required observation ordering. A writer
   starting later may insert into the heap, but cannot publish a new indexed
   root through this guard and cannot commit its index insertion before release.
3. DELETE and HOT can proceed without this index interlock. A fresh VM probe is
   therefore necessary on each result page, and each sparse/frontier scalar root.
   No all-visible bit is cached across page callbacks, statements or rescans.
   Concurrent committed-before-snapshot deletes must be observed through the
   PostgreSQL snapshot/VM ordering; later deletes retain the old visible version.
4. VM-false candidates go through `table_index_fetch_tuple`, not a raw heap-TID
   lookup. It uses the active MVCC snapshot and a private mutable copy of the root
   TID; HOT redirects are followed. Exact protected predicate membership means
   text detoasting/tokenization is not repeated. Non-MVCC continuation is an error.
5. VACUUM cannot complete the index-retirement phase while the guard protects a
   source, and PostgreSQL frees an indexed heap slot only after index cleanup.
   A root redirected along a live HOT chain still represents one row. A dead root
   not yet index-cleaned must not be certified by an all-visible heap page. These
   are native visibility/liveness obligations, not consequences of popcount alone.
6. PG errors/cancellation unwind through existing guarded boundaries and
   transaction resource cleanup. Pure-core errors explicitly release the new
   lock before propagation. Result and counter arrays are written to C only after
   the entire protected scan completes. Rescan clears counters, resources and
   runtime fallback state. No private state is written to disk or restored after
   crash; fresh execution reacquires all facts.

This is a deliberately coarse **generation-stability** guard. It is not the old
owner-buffer-pin protocol, and it is not a lock-free count. Once acquired, it can
block subsequent writers and VACUUM for the whole query. Acquisition is
nonblocking only for tag 0; the preexisting structural ShareLock can still wait
behind maintenance. Writer latency/throughput under sustained counts is a
mandatory tradeoff measurement before promotion.

The core finite model explores supplied snapshot outcomes, guard/read/VM/cleanup
schedules and cancellation, and finds counterexamples for omitted protection,
copy-before-protection, stale VM and partial publication. It assumes PostgreSQL's
reclamation horizon and correct fallback; it is not an independent MVCC engine.

## Eligibility, activation and migration

The existing planner restrictions remain: top-level ordinary `COUNT(*)`, one
non-inherited permanent heap relation, one exact constant PIN predicate, no
residual/RLS/security-barrier/parameterized/serializable/recovery special case.
The same retained PostgreSQL aggregate is used when runtime eligibility changes.
Phrase/prefix queries and ranked/row-returning plans retain ordinary execution.
Costing still retains the baseline heap/index cost and credits only avoided
aggregate transition cost; the harness records actual path selection, not GUCs.
The new path is serial even if the older count-worker GUC is nonzero.

On an isolated qualification server, after installing matching binaries and
restarting preload:

```sql
SET pin.enable_grouped_storage = on;
-- create the PIN index here, or VACUUM an existing index to build its snapshot.
SET pin.enable_grouped_scan = on;
SET pin.enable_count_fastpath = on;
SET pin.enable_count_vm = on;
SET pin.enable_grouped_count = on;
EXPLAIN (ANALYZE, BUFFERS)
SELECT count(*) FROM ONLY documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha AND bravo');
```

All gates retain their default-off policy. The new GUC is superuser-settable.
There is no disk, analyzer, query-format or extension-SQL migration. Existing
compatible complete G9 snapshots are reused; indexes without one fall back.
The private C/Rust counter ABI changes together from 8 to 18 words, including old
parallel count state. A matching rebuilt library and server restart are required;
never mix old and new object files or assume a running preloaded backend changed.
Hot-standby PIN index reads remain explicitly unsupported; generic replay is not
a standby generation-conflict protocol.

## Measurement and qualification

Added counters distinguish grouped result pages/roots, scalar roots, fresh VM
probes, certified roots, heap fetches/matches, private index-page reads and copied
payload bytes, decoded term offset bytes/payloads and candidate/live heap pages.
The decoded fields cover sealed evaluator work only, not canonical sparse/frontier
payloads or liveness decode bytes. GIN internal decoded bytes are **unavailable**,
not zero. Do not sum inclusive EXPLAIN buffers across parent and child nodes.

Run native qualification with `tools/g9_count_qualification.sh` on the pinned
PG18.6/pgrx0.19.2 toolchain. G0 schedules ordinary and test-hook builds; the latter
also enables both optional frontier policies. Its independent PostgreSQL-simple
identity oracle covers fresh, short/long deltas, Boolean/NOT, phrase fallback,
NULL/empty input, actual HOT, DELETE, indexed updates, actual asserted CTID reuse,
VM on/off, own writes/rollback, cached-plan/runtime/memory fallback, RLS and rejected
aggregate shapes. Test-hook mode adds concurrent writers/readers, repeatable-read
snapshots through VACUUM, contention fallback, errors, cancellation/termination
before scanning and after partial visibility work, and immediate crash/restart
with a post-checkpoint committed witness. Native outcomes remain unobserved here.

A paired experiment, from a clean committed source tree, with a disposable
database and the local database PID namespace:

```sh
python3 tools/g9_count_bench.py \
  --bindir "$(pg_config --bindir)" --pin-so "$(pg_config --pkglibdir)/pin.so" \
  --expected-revision "$(git rev-parse HEAD)" --same-host \
  --host-note 'record hardware, governor, storage and other load' \
  --output .artifacts/grouped-count-paired \
  --rows 20000 1000000 --blocks 6 --queries 100
```

The driver retains old PIN/new PIN/GIN in every block and uses all six balanced
orders. Each class uses the same heap and snapshot; full ordered `(id,ctid)`
streams are compared against an independent forced-sequential PostgreSQL-simple
oracle outside timing. Source/lockfile/binary hashes, exact settings, raw SQL,
plans, per-query client latencies, per-batch schedstat CPU, throughput, index sizes,
build CPU/time/WAL and one-index-only insert/VACUUM CPU/WAL are retained. Rare,
selective AND, broad AND/OR, NOT, count, phrase, row output and ranked top-k remain
separate. New deltas do not turn the rare class into a broad one.

Ranked controls use the **same PostgreSQL `ts_rank_cd(..., 0)` expression and
`ORDER BY score DESC,id LIMIT 20`** for both engines. There is no score-aware PIN
pruning claim. Broad row outputs are delivered to persistent psql in every timed
mode; only client file output is redirected to `/dev/null`, after full output-file
comparison. Latencies are client round trips, not isolated executor time; short
CPU batches and insufficient tail samples are flagged. Build order alternates
between corpora. WAL measurements are cluster-wide and require an otherwise
quiescent host. The new deterministic corpus is varied synthetic ASCII, **not a
natural-language production corpus**.

Unresolved: exact-head Rust/Clippy/rustfmt/PG ABI tests, live recovery/standby guard
qualification, full allocation-failure/RSS and cold-cache tests, native concurrent
writer p95/p99/throughput, durable incremental sealing and bounded merge debt,
rare-path copy reduction, exact-score top-k bounds, write amplification reduction,
natural-language corpus qualification, and a directly matched TIN run. Report
actual wins *and regressions*, not an assumed 10x capability, before enabling this
path for production.
