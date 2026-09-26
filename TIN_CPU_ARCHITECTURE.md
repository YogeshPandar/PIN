# PIN CPU architecture, 26 September 2026

## Goal and measurement boundary

Target a credible 10x speedup over PostgreSQL GIN on matched full-text workloads,
including Boolean search, phrases, ranked top-k, and continuous writes. Preserve
PostgreSQL transaction visibility, WAL, recovery, VACUUM, replication, joins,
and the current PIN query features. The target is a product goal, not a measured
claim. TIN's published large-corpus results use different hardware, corpora,
queries, and ranking, so no ratio in this document compares PIN directly to TIN.

The current paired fixture uses PostgreSQL 18.6 on a GCP VM, 20,000 initial
documents plus 4,096 appended documents, with the same SQL result identities
checked for PIN and GIN. The merged PR #26 baseline after rebuild measured
ordinary broad row retrieval at 6.47 ms PIN versus 8.08 ms GIN, phrase COUNT at
79.57 ms versus 218.07 ms, and ranked top-k at 272.03 ms versus 273.17 ms of
backend CPU per query. The ranked query deliberately uses the same PostgreSQL
`ts_rank_cd` expression on both engines, so both score a large candidate set in
the heap. Those are warm, serial measurements, not production p99 figures.

## What the sources imply

PostgreSQL's [index-scanning contract](https://www.postgresql.org/docs/18/index-scanning.html)
lets an access method return CTIDs with exact membership, or candidates with
heap predicate recheck. It leaves heap visibility to PostgreSQL. Its bitmap API
batches TIDs but cannot return indexed document values. The pinned PostgreSQL
18.6 [`heapam_index_fetch_tuple`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/heapam_handler.c#L115-L160)
still follows HOT and snapshot rules. The
[visibility map](https://www.postgresql.org/docs/18/storage-vm.html) can help
skip heap visibility only for all-visible pages under the appropriate ordering
and locking protocol. PIN cannot simply count index hits or return index rows
as visible SQL rows.

PlanetScale describes TIN's native CTID postings, page and offset bitmap
operations, per-segment liveness, immutable and mutable segments, and background
merging in its [TIN architecture article](https://planetscale.com/blog/introducing-tin).
TIN also supplies index-native phrase and BM25 top-k work. Its published index
was larger than GIN's on the cited corpus while queries were faster. This
supports spending space to remove CPU work, but does not prove that enlarging
PIN's index will improve a query. The current PIN page chains, owner resolution,
page copies, validation, and heap scoring remain separate costs to measure.

## Evidence from this iteration

The baseline `strace` of 40 warm PIN phrase queries counted 50,320 backend
`lseek` calls and no file reads. The index read path repeatedly asks
`RelationGetNumberOfBlocks` for a bound, which reaches the PostgreSQL storage
manager. A callback-local cache and bounded C read reduced `lseek` to 200 per
40 queries, but did not materially improve backend CPU in paired phrase runs.
Both changes were reverted because the extra FFI and concurrency contract
were not justified by the measured effect.

The new default-off `pin.enable_phrase_positions` path validates inline indexed
positions and proves a root-level phrase before emitting a bitmap TID. It keeps
heap visibility and falls back to heap predicate recheck for fragmented owners.
On the rebuilt fixture, an initial paired run measured `"bravo charlie"` at
27.53 ms PIN backend CPU with positional proof, 79.76 ms with the prior PIN
recheck, and 218.95 ms GIN. Full delivered `id,ctid` streams matched. This is
about 8x faster than GIN for this first phrase path. A second pass that
validates and selects indexed positions in one loop measured 22.47 ms versus
218.76 ms GIN on the same phrase, about 9.7x. It does not carry over to broad
rows or ranked top-k.

## Next architecture changes

1. **Move phrase and span truth into a complete positional index view.** The
   current proof covers inline owners only. Fragmented documents still tokenize
   heap text. Extend the indexed positional representation so every supported
   phrase, span, and position constraint can be decided without text analysis.
   Keep a recheck path whenever an indexed proof is incomplete.
2. **Build an index-native BM25 top-k executor.** Store exact document length and
   per-term frequency with postings, plus corpus statistics with transactionally
   safe publication. Use upper bounds to skip blocks whose maximum score cannot
   beat the current top-k threshold, then fetch and validate only surviving
   CTIDs. The SQL planner must select this path only when it can preserve query
   ordering, heap visibility, joins, and tie rules. Current `ts_rank_cd` SQL is
   a benchmark control, not BM25, so a new BM25 path needs its own equal-function
   oracle. This is the main route to changing the ranked result, where PIN and
   GIN currently tie.
3. **Make posting traversal batch-oriented.** Decode CTID page/offset groups
   into bounded arrays and intersect or union whole groups before resolving
   owners. Reduce repeated owner loads, page validation, buffer calls, and Rust
   to C crossings per candidate. Measure cycles, copied bytes, buffer hits, and
   `lseek` separately. A page-sized copy can be worthwhile if it enables much
   less work downstream; avoiding copies alone is not the goal.
4. **Maintain immutable and mutable segments without stale visibility.** Use
   stable CTID coordinates for posting groups and publish new segments through
   PostgreSQL WAL. Merge immutable groups only after reader pin and VACUUM
   conditions permit reclamation. Keep old snapshots, aborted writes, HOT,
   standby replay, and crash recovery in the native test matrix. PIN already has
   experimental grouped storage, so this step should evolve measured paths
   rather than replace them by assumption.

Every step needs paired full-result oracles against the sequential SQL predicate,
normal and dirty heap lifecycle tests, backend CPU and p95/p99 latency, index
buffer traffic, write WAL volume, and concurrency/recovery qualification. A
speedup on a count-only or exact bitmap path does not establish ranked or row
retrieval parity.

The next architecture specification is now tracked in [pin_next.md](pin_next.md).
Its A3 positional bridge is being implemented first: selected term views over
inline/fragmented PD02, shortest-list phrase anchoring, and bounded reusable
payload scratch. Packed primary storage and native SQL BM25 remain outstanding;
this bridge alone does not establish the full feature/performance target.
