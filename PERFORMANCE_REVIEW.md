# Pin performance and architecture review

**Date:** 22 September 2026  
**Status:** research and local development evidence; Pin is not production certified  
**Current measured code:** `eaad9c22a5ab47d1fc64a7fa718792183bdd08d8`

## What the benchmark actually says

The latest comparison uses one 20,000-row synthetic `pin_g6_bench` table,
PostgreSQL 18.6 with assertions, a four-vCPU VM, warm tmpfs, four clients,
two threads, and three alternating three-second samples per query and engine.
Both Pin and a `simple`-configuration PostgreSQL GIN expression index return
the same row IDs for each fixed query. Plans force serial bitmap scans. JIT,
Pin count shortcuts, and parallel scans are off. The table reports median
queries per second (QPS) and Pin's median per-sample p95 service latency.

| Query | Pin QPS | GIN QPS | Pin / GIN | Pin p95 ms |
| --- | ---: | ---: | ---: | ---: |
| `alpha` | 904.86 | 806.51 | 1.12 | 4.929 |
| `rareplanet` | 33,724.93 | 57,139.90 | 0.59 | 0.224 |
| `alpha AND beta` | 460.96 | 701.70 | 0.66 | 9.751 |
| `alpha AND rareplanet` | 7,030.17 | 30,422.87 | 0.23 | 0.797 |
| `alpha OR rareplanet` | 583.92 | 792.96 | 0.74 | 7.642 |
| `"beta gamma"` | 13.85 | 2.95 | 4.70 | 293.213 |

The phrase result is specific to this fixture and query plan. Pin's phrase
path still checks candidate text and remains slow in absolute terms. GIN is
faster on four of six cases, with the largest gap on selective AND. There is
no measured TIN comparison. PlanetScale's published measurements use much
larger corpora, different hardware, and often ranked top-k queries; those
numbers cannot be divided into these local QPS values to make a valid claim.

The preceding page-skip candidate measured `alpha AND rareplanet` at median
4,462.78 Pin / 29,501.15 GIN QPS. Skipping direct pages whose highest owner
precedes the seek target raised Pin to 7,022.44 / 31,109.65 GIN QPS in a
matched selective run. Pin rose about 57% in raw QPS, yet remained about 4.4x
slower than GIN. The full six-query rerun above agrees on the Pin result.
An earlier change deferred direct TID reads until a Boolean match survived;
its broad benchmark moved only slightly relative to GIN and is not evidence
of a material general speedup.

All samples, SQL, plans, row-identity checks, environment and test logs are
archived in [the owner-merge run](docs/runs/2026-09-22-owner-merge/README.md).
The prior direct-segment checkpoint is in
[its run record](docs/runs/2026-09-22-direct-segments/README.md). The current
fixture does not cover sustained writes, cold storage, long-lived snapshots,
large vocabularies, WAL volume under load, hot standby reads or ranking.

## How Pin currently works

Pin is a PostgreSQL index access method. Inserts publish canonical owner
records and searchable mutable postings. A canonical owner identifies an
index page, slot and never-reused incarnation, and maps to the heap root TID.
VACUUM retires owners before PostgreSQL may reuse a heap slot. Compaction can
turn mutable postings into sealed pages under structural and writer barriers.
Generic WAL protects page changes and recovery. The query executor merges
ordered owner streams and sends candidate heap TIDs to PostgreSQL; the heap
still decides MVCC visibility.

The new **opt-in** tag-9 direct sealed pages copy a heap TID and local live bit
next to every compressed owner. Simple terms can emit those TIDs without
resolving every owner. Boolean queries still intersect and union by canonical
owner, then use copied TIDs for survivors. The latest change skips a direct
page's second decode when its validated last owner is behind an AND seek
target. Page validation still decodes and checks the entire page on load.

That identity choice protects correctness. A pure-engine oracle found that
two distinct owner incarnations can use the same heap coordinate; a proposed
TID-only Boolean intersection produced a false AND hit and was reverted.
Any replacement must provide a generation or liveness proof for reused TIDs.

## How this differs from TIN's public design

PlanetScale describes TIN as using physical `ctid` values directly, grouping
heap pages into 256-page bitmap units, then using offset bitmaps within each
page. Its public article also describes per-segment liveness, mutable and
immutable segments, background merges, exact term counts, vectorized Boolean
operations, visibility-aware custom scans and BM25 top-k execution.
[PlanetScale TIN architecture](https://planetscale.com/blog/introducing-tin)
is a design reference, not Pin code or a specification of every TIN invariant.

| Concern | Pin today | TIN public description |
| --- | --- | --- |
| Primary posting identity | Canonical owner page/slot/incarnation | Physical heap `ctid` |
| Sealed representation | Compressed owner stream plus optional copied TID | Heap-page and offset bitmaps |
| Boolean execution | Ordered owner cursor merge; direct-page range skip | Page-mask pruning, then offset bitmap operations |
| Deletion state | Canonical owner plus copied per-term direct live flags | Per-segment liveness bitmap |
| Immutable storage | VACUUM compaction with retained page chains | Mutable and immutable segments with background merges |
| Exact count and ranking | Count experiments remain gated; no ranked SQL top-k | Visibility-aware count and BM25 top-k custom scans |

**Yes, the implemented Pin architecture is materially different.** The original
[Pin plan](pin_plan.md) already proposes page groups, segments and ranked
execution, but the implemented index has not reached that design. Copying TIDs
into sealed pages improved one hot path while retaining two identities and
per-term liveness. It cannot deliver the main work-elision benefits of TIN's
page-group bitmap layout by itself.

PostgreSQL's [index-scanning contract](https://www.postgresql.org/docs/18/index-scanning.html)
allows `amgetbitmap` to return heap TIDs for later heap checks; it does not
grant snapshot visibility from an index bit. Its
[index-locking rules](https://www.postgresql.org/docs/18/index-locking.html)
also matter when heap slots can be removed or recycled. Pin must preserve
these contracts during any storage redesign.

## What to build next

1. **Generation-safe page-group postings.** Add a new opt-in sealed segment
   format with 256-page group masks and compact offset masks. Keep a segment
   generation or owner-incarnation mapping so a reused TID cannot activate an
   old posting. Prove SQL equality across UPDATE, HOT chains, DELETE, VACUUM,
   crash/replay, compaction and TID reuse before enabling it by default.
2. **Shared segment liveness.** Store retirement once per segment rather than
   once per term. VACUUM must durably clear liveness before heap slot reuse;
   readers need a protocol for old and new segment generations under the
   existing structural barrier and WAL journal.
3. **Prune before decoding.** Intersect 256-page masks for AND; skip offset
   blocks on empty groups. Start with a portable scalar kernel and compare it
   to an independent owner-aware oracle. Add SIMD only if profiles and matched
   SQL runs justify it. Preserve bounded private memory and cancellation.
4. **Measure write and space costs.** Compare index build, insert/update/delete,
   VACUUM, compaction, WAL bytes, index size, memory, and recovery time against
   GIN. A read gain that stalls writes or inflates the index is not a win.
5. **Visibility and ranked top-k.** Introduce visibility-map count and ranked
   custom scans only after their snapshot, VM pin, planner, ranking and replay
   proofs are complete. Exact heap-checked bitmap search remains the fallback.
6. **Use a fairer benchmark.** Add larger real corpora, low/high selectivity,
   mixed reads and writes, cold and warm cache, queries per second, p95/p99,
   bytes read, WAL, and build costs. Compare the same SQL semantics and verify
   row identities. A direct TIN comparison requires an actual TIN instance
   under matched conditions.

The immediate engineering priority is item 1. Cursor micro-optimizations have
helped specific queries, but the remaining GIN gap follows from decoding
owner streams and validating pages before Pin can discard irrelevant groups.
No current result supports a claim that Pin is the world's fastest extension.
