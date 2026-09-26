# PIN primary index v2: architecture decision

Status: implementation in progress, not a measured speedup or a deployable index.
The first checked codec is `crates/pin-core/src/primary/mod.rs`; PostgreSQL does
not yet read or write it. This decision supersedes the idea that another local
grouped-bitmap read optimization will deliver TIN-like performance. The fuller
dependency and correctness analysis remains in `pin_next.md`.

## Why PIN is still behind the target

PIN already stores heap TIDs, groups 256 heap pages, and prunes page masks.
That is a useful resemblance to TIN, but it is not the whole design. PIN first
builds and maintains canonical owner/posting chains, then builds grouped data
as a supplemental view. Its grouped reader still loads complete selected term
records and uses canonical dictionary identities and mutable suffixes. Query
forms that need ranks or full positional data leave the fast grouped Boolean
path. The recent direct grouped build experiment made byte-identical indexes
and showed no stable build CPU win; it did not change query execution.

On the repository's paired 16,000-row, 8,000-term fixture, packed grouped PIN
used about 97-99 microseconds of backend CPU for selective AND versus about
118 microseconds for stored GIN, and 1.16 milliseconds for a broad count versus
1.87 milliseconds for GIN. Those are useful wins, but not 10x. The earlier
ranked comparison used the same heap-side `ts_rank_cd` on both indexes and was
near parity. PIN has no index-native SQL BM25/top-k executor. These are
different workloads from TIN's published large-corpus benchmarks, so no
PIN/TIN ratio can be computed from them. [Local evidence](docs/runs/2026-09-26-packed-direct-combined/README.md).

The CTID alone is not a differentiator against PostgreSQL GIN: PostgreSQL's
own index types also return TIDs. TIN's documented advantages include the
two-level bitmap representation, skipping non-surviving offset payloads,
index-native count and ranked execution, separate positional/frequency data,
and background mutable/immutable segment maintenance. See PlanetScale's
[architecture and benchmark article](https://planetscale.com/blog/introducing-tin)
and [search-engine anatomy](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine).

## The replacement

New indexes will eventually use an incompatible v2 format. Old indexes retain
their v1 reader until explicit REINDEX migration. A v2 index has one
authoritative CTID/page membership representation; it does not write full
canonical posting chains merely to support a nominal fallback.

```text
heap build scan -> analyzed term/CTID/TF/position records -> PostgreSQL spill sort
             -> immutable segment manifest
                  -> sorted term directory and exact physical counts
                  -> 256-page presence summaries
                  -> independently addressable per-page offset containers
                  -> document lengths and term-frequency blocks
                  -> selected-term position blocks
                  -> liveness and publication generations
new writes -> bounded mutable component -> sealed immutable segments -> merge
```

The first codec uses a 76-byte term/group header and 12-byte descriptors for
each present heap page. Its largest directory is 3,148 bytes. A descriptor
addresses one sparse or dense offset container separately. The reader can
reject a heap page using the 256-bit summary without fetching its offset
container. Physical allocation must preserve this property: packing unrelated
containers into the same PostgreSQL buffer page could erase the I/O win, even
though the codec calls only for the selected extent. A native benchmark must
count actual buffer reads, copied bytes, and decoded bytes.

The term directory will use logical term ordinals, not canonical dictionary
page offsets. Sorted vocabulary blocks must support bounded exact, prefix,
range, fuzzy, and wildcard expansion. Membership, TF/document length, and
positions are separate streams so a Boolean query does not load score or span
data. A sparse conjunction iterates its rare lead and probes only candidate
pages; a dense Boolean query combines page masks before offset containers.

Complete-document publication, owner incarnation, VACUUM liveness, and a
generation-pinned manifest remain required. A reused heap slot must never join
postings from different document versions. Writers publish a bounded mutable
suffix atomically with WAL. Sealing and merging prepare output outside the
short publication critical section, retain old extents while readers need
them, and publish a new manifest only after its payload is durable. These are
correctness requirements, not optional performance features.

## Three execution consumers

1. **Rows:** use ordinary PostgreSQL bitmap/index scans when those plans are
   cheapest; emit exact CTIDs in heap order and let PostgreSQL check MVCC and
   residual quals. Do not make a custom executor the default for every query.
2. **Count:** keep page masks through counting. Use exact posting counts only
   when disjointness, liveness, and visibility certificates justify it. Use
   PostgreSQL's visibility map only under a reviewed protocol; dirty pages
   still need heap checks. The VM is conservative and is cleared on writes.
3. **Ranked top-k:** first implement exhaustive native BM25 as a correctness
   oracle, with statement-pinned statistics, TF, document length, snapshot
   visibility, security quals, and deterministic ties. Then add conservative
   block score bounds so uncompetitive blocks never load TF/position data or
   visit the heap. A generic `amgetbitmap` cannot itself return ordered scores,
   so this needs a separately qualified planner/executor path.

PostgreSQL's [index AM scan contract](https://www.postgresql.org/docs/18/index-functions.html)
and [visibility-map contract](https://www.postgresql.org/docs/18/storage-vm.html)
set the boundaries for these paths. Count and rank shortcuts must not infer
snapshot visibility from index membership.

## Implementation order and acceptance

| Gate | Deliverable | Decisive evidence |
| --- | --- | --- |
| V2a | Selected-page container codec and native extent reader | Independent set oracle; malformed-format tests; selected physical reads and copied bytes fall with page selectivity |
| V2b | Direct external-sort build and v2 manifest | No canonical posting-chain build; complete WAL/crash/restart proof; build CPU/WAL/peak space |
| V2c | Mutable writes, VACUUM, sealing, reader retention | Update/delete/HOT/slot reuse and concurrent reader/writer oracles; bounded p99 write stalls |
| V2d | Exact row and count consumers | Same SQL identities and MVCC visibility as independent heap scans; paired CPU by rare/dense/dirty-page class |
| V2e | Indexed positions, spans, fuzzy and wildcard expansion | Semantic oracle plus measured selected position bytes, expansion limits and fallback reasons |
| V2f | BM25 top-k and safe pruning | Exhaustive score/order oracle, eligible-row proof, and measured scored-candidate/block reductions |

The performance goal is a matched-workload comparison, not a universal 10x
promise for every query. Report p50/p95/p99 latency, backend CPU, bytes read,
WAL, index size, build time, and concurrent write latency on the same corpus,
PostgreSQL version, hardware, SQL result set, and warm/cold state. Compare to
GIN using equivalent predicates; compare BM25 only to a BM25 oracle, with
`ts_rank_cd` kept as a separate baseline. Run TIN itself when access to the
same engine and corpus is available. A codec-only pass cannot establish a
search speedup.

## Current code boundary

`pin-core::primary` now checks a proposed v2 directory and sparse/dense
per-page containers without PostgreSQL pointers. It deliberately has no
manifest, WAL, liveness, or SQL integration. No existing index is changed by
this branch. The next required code step is a PostgreSQL-backed extent reader
that measures whether selective page access saves physical buffer work on a
paired query. If it does not, revise the allocation layout before building
the rest of v2 on top of it.
