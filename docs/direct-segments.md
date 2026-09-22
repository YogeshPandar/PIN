# Direct heap coordinates in sealed postings

Status: experimental, opt-in, self-reviewed. The branch changes the index page
format. See `docs/api-evidence.md` DT01 and the matched runs under
`docs/runs/2026-09-22-direct-segments/`.

## Why this step

The prior posting format stores `OwnerRef` (index page, slot, incarnation), then
reads a canonical owner page to obtain the heap TID for every surviving match.
Software profiling found posting decoding and page copies among the largest
index-side costs. On a 20,000-row fixture, an exact common-term bitmap still
ran at about 458 QPS versus 790 QPS for GIN. One test counted more than ten
owner-page reads for 1,800 documents. PlanetScale's public
[TIN architecture article](https://planetscale.com/blog/introducing-tin)
explains why physical TIDs and heap-page grouping can avoid this mapping cost.
It does not disclose TIN's full implementation.

## Format and read path

Experimental tag 9 pages contain ordered, compressed owner identities and
parallel fixed-size heap coordinates. Each coordinate has a durable live bit.
The page header also records the first and last owner identities; validation
checks every encoded owner and both endpoints. A simple term scan can iterate
live coordinates without a second posting decode or per-result owner lookup.
Boolean intersections still merge owner identities to distinguish incarnations;
they use the copied TID after merging. Mutable insertion still uses the existing
owner-publication protocol and remains searchable immediately.

A new page is built under the existing writer and exclusive structural barriers.
The compactor writes each output page under a recovery journal, then atomically
publishes the replacement chain and retiring journal in one Generic WAL batch.
After publication, old pages wait for reader quiescence before reuse. This
retains the existing bounded page scratch and crash protocol.

## VACUUM and visibility

PostgreSQL 18.6 calls index bulk deletion before its heap vacuum phase can
recycle an indexed tuple's line pointer. The Pin bulk-delete pass first marks
canonical owners dead, then clears their copied coordinates in every direct
page before returning. A partial pass may leave stale coordinates, but cannot
authorize heap-slot reuse. Retrying the pass clears copies of already dead
canonical owners. All updates use the existing Generic WAL page commit. A
long-lived query can still encounter an old coordinate while PostgreSQL checks
its statement snapshot against the heap. No index bit certifies MVCC visibility.

This scheme duplicates liveness per term. VACUUM work therefore rises with
posting count. The next storage design should use a shared per-segment liveness
bitmap, keyed by a heap-page group and a document incarnation. It must prevent
a new tuple at a recycled TID from activating an old segment's postings. The
public [TIN article](https://planetscale.com/blog/introducing-tin) describes
256-page group masks and per-segment liveness; these are design references,
not implementation evidence for Pin.

## How to exercise it

A superuser can enable creation of direct pages for a maintenance session:

```sql
SET pin.enable_direct_tid_segments = on;
VACUUM (PARALLEL 0, INDEX_CLEANUP ON) my_table;
```

The setting controls future compaction. Existing direct pages remain readable
when it is off. An older binary rejects tag 9 pages. Rebuild indexes with the
setting off before downgrading. The SQL predicate continues to use PostgreSQL
heap visibility, HOT handling, lossification and residual filters.

## Remaining gates

The tested fixture is small, warm, synthetic, serial, and stored on tmpfs.
Direct pages have not been measured under sustained writes, cold storage,
replication replay or high term cardinality. The current layout keeps owner
identities and copies liveness per term, so it is not yet the intended
page-group bitmap architecture. Ranked top-k SQL and read-replica queries remain
unsupported. Independent storage and FFI review remains open.
