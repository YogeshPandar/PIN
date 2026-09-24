# G9 generation-safe grouped storage

Status: logical and physical grouped storage with a default-off PostgreSQL adapter.
Native validation of the new adapter and independent storage review remain pending.
This document specifies the logical format and its conditional correctness argument.
See [g9-integration.md](g9-integration.md) for host APIs, publication, activation,
rollback, memory budgets and the exact limits of locally executed tests.

## Implemented boundary

`crates/pin-core/src/grouped/` contains checked borrowed bitmap records,
immutable generation membership, shared clear-only liveness, owner-checked
term sealing, a scalar Boolean evaluator, and bounded logical compaction.
`crates/pin-kernels/src/grouped.rs` contains safe fixed-mask operations. The
logical kernel adds no unsafe block or PostgreSQL pointer and uses caller-owned
scratch. Caller-owned input/output storage is not free memory.

`mutable/page_grouped.rs` and `mutable/grouped/` add physical framing, a catalog,
complete-owner capture, shared retirement and journaled publication/recovery.
`pin-pg/src/grouped.rs` and its C sort bridge connect this core to build, VACUUM
cleanup and bitmap scans. The new path retains canonical owners, legacy postings
and positions. Both host settings remain off by default. It is neither completed
legacy-storage removal nor a performance qualification.

## Identity comes before bit operations

A segment contains complete documents, not arbitrary independent term flushes.
Within one relation generation and segment identity, each heap root coordinate
maps to exactly one canonical owner incarnation. The mapping is immutable.
Different occupants of a reused coordinate must belong to different segments.
Boolean expressions are evaluated inside that identity boundary; only completed
matches may be unioned across segments. Intersecting term masks across segment
identities is forbidden. Merging cannot silently collapse different owners at
the same coordinate. Retain separate segments or prove the old owner retired.

`Members::encode_posting` checks every root/incarnation pair against the shared
membership before discarding per-posting owners. It inherits the segment key.
The low-level `encode_bitmap` is a geometry codec, not proof of canonical owner
provenance. The host must reserve durable fresh segment IDs, validate canonical
owner assignments, and seal complete term coverage under a writer barrier.
Those host obligations are not established merely by matching numeric keys.

Liveness can only clear for an existing segment. Retirement requires the exact
root and incarnation; wrong-generation requests cannot clear a reused slot.
Fresh liveness for a fresh merged segment contains its surviving owners only.
This is index retirement, not snapshot visibility or an exact SQL row count.

## Logical format version 1

All integer fields are little-endian. No Rust struct layout or aligned cast is
used. A common 72-byte header precedes the record body:

| Byte offset | Width | Field |
| --- | --- | --- |
| 0 | 4 | `PNG9` magic |
| 4 | 2 | Version, currently 1 |
| 6 | 1 | Kind: posting 1, liveness 2, membership 3 |
| 7 | 1 | Reserved, zero |
| 8 | 4 | Exact total record byte length |
| 12 | 4 | 256-page-aligned heap block base |
| 16 | 8 | Nonzero relation generation |
| 24 | 8 | Nonzero segment identity |
| 32 | 2 | Checked `HeapLayout` maximum offset |
| 34 | 2 | Number of set page bits |
| 36 | 4 | Membership count, zero for bitmap records |
| 40 | 32 | Four page-mask words, ordered by heap block |

A bitmap has one four-byte directory entry per set page bit: packed payload
start (`u16`), payload byte count (`u8`), and a zero reserved byte. Page rank is
computed from four cached word prefixes and one popcount. Each payload uses
one to 64 bytes; bit zero represents heap offset one. Posting payloads omit
zero high bytes. Liveness preserves its original widths and directory even
when every bit on a page is retired. The last reserved heap block is rejected.
Unknown versions, duplicate coordinates, malformed packed extents, reserved
fields, trailing bytes and out-of-domain offset bits are rejected.

`Bitmap::open` validates the header and directory without decoding offsets.
`offsets(page)` validates and decodes only that payload into fixed scratch.
`validate_all` is an explicit eager maintenance/build check. Skipped corrupt
payloads are not certified by a pruned query; physical checksum and publication
validation remain part of the host gate.

Membership stores sorted unique 16-byte records: heap block (`u32`), offset
(`u16`), zero reserved field (`u16`), and canonical incarnation (`u64`). It is
shared across terms. The maximum bitmap record is 17,480 bytes, and the largest
membership record at the supported 512-offset domain is 2,097,224 bytes. These
are logical records, not 8 KiB PostgreSQL pages or WAL-atomic extents.

## Scalar correctness argument

For a fixed valid segment, a coordinate denotes at most one immutable owner.
Let U be its live membership, and T_i the coordinate set for term i. The leaf
result is T_i intersect U. AND, OR and difference use the corresponding set
operations followed by intersection with U. NOT is U minus its child result.
Induction over the checked topological program therefore gives the same result
as evaluating that Boolean expression over the segment's live owners. This
argument depends on complete, canonical term coverage established at sealing.

A page summary is conservative: AND intersects page masks; OR unions them.
Difference retains the left summary because distinct offsets can share a page.
NOT retains the live universe summary because a term can occupy only part of a
page. Thus pruning never removes a page containing a true result. A reverse
DAG dependency pass avoids descendants of page-empty subexpressions and avoids
unreferenced nodes. Retired pages skip term decoding entirely.

The evaluator supports up to 64 nodes, uses 6,144 bytes of caller-owned scratch,
and emits heap-ordered page/offset masks. It does not look up each matching TID
in the membership map. Cancellation is checked at entry and per candidate page;
a callback error invalidates all output, including already emitted pages.
`QueryStats` counts decoded term payloads/bytes and candidate membership, not
physical I/O, visible rows or time. The independent oracle tests compare owner
sets, rather than implementing a second copy of the bitmap algorithm.

## Compaction and retirement argument

`MergePlan` accepts at most 16 pinned private source snapshots with the same
relation generation, heap group and layout, and requires a fresh target ID.
Its fixed cursor array visits live members in heap order. Live occupants with
different incarnations at the same coordinate are rejected. Overlapping copies
of the same owner may be deduplicated only when their complete canonical term
coverage agrees; partial per-term copies are not valid source segments.

For each term, the target is `union_i(posting_i intersect liveness_i)`. Filtering
must happen before union. Suppose an old occupant at coordinate c had term A,
was retired, and a new occupant at c has only B. Unioning postings first and
then applying the union of live masks would preserve old A at the new B slot.
Source-local filtering removes A before the new coordinate is retained. A
regression test covers exactly this false-AND scenario.

The private-image `retire` operation is idempotent and clear-only. It validates
the requested incarnation and selected page before mutation. Reapplying a
retirement changes nothing. Private image replay tests do not establish WAL
ordering, flush policy, interrupted publication or PostgreSQL crash recovery.
Compaction checks cancellation through long retired runs and between page work.
Cancellation can leave a partial private target, which must never be published.

## Performance limitations still requiring design work

The scalar optimization is avoided term offset decoding after page pruning.
There is no measured SQL speedup in this PR. `SegmentGroup::open` eagerly checks
all membership and liveness; the borrowed view can be reused while its backing
snapshot remains valid. Repeating that full open on every cold query could
negate pruning. The physical reader uses separately addressable catalog entries and validates
selected records without reopening the complete membership roster on each scan.
It still copies selected record fragments; decoded-byte reductions are not a
measured physical-I/O reduction. Cache strategy and I/O costs remain benchmark gates.

The membership representation uses 16 bytes per owner once per segment, not
per term. That is not a claim of optimal index size. Singleton/sparse offset
codecs, ownership transfer during extent merging, dirty-group liveness elision,
SIMD, exact counts and ranked top-k remain later measured work. HeapLayout
supplies the domain; the implementation does not assume every page has 291 or
512 actual tuples. Private byte buffers and output accumulation must be charged
to the host memory budget in addition to the fixed query scratch.

## PostgreSQL integration gates

1. Seal complete owner coverage under the existing structural and writer
   barriers; create immutable generation mapping and shared liveness first.
2. Add bounded physical extent pages, reserve fresh durable segment identities,
   WAL-log an unreachable build journal, then atomically publish manifest
   coverage. Keep old coverage until that switch; keep old formats readable.
3. VACUUM must retire canonical owners and every published segment's shared
   liveness before `ambulkdelete` returns and permits heap slot reuse. A failed
   or interrupted pass must be replayable and idempotent. Follow WAL ordering;
   this is not an instruction to perform an `XLogFlush` for every cleared bit.
4. Readers capture a manifest under a structural barrier; liveness reads need
   the matching buffer protocol. No borrowed shared-buffer pointer survives
   unlock. Reclaim source extents only after readers quiesce.
5. Preserve immediately searchable mutable writes and owner-aware fallback for
   mixed storage. Do not AND partial results from independent term migrations.
6. Add a default-off SQL setting only when the new path exists. Preserve heap
   visibility checks, bitmap recheck flags, MVCC-only scans, interruption,
   memory budgets and the server's generic WAL registration bound.

The physical core and gated adapter implement these protocols. Native execution,
crash qualification and independent review remain open acceptance gates. Do not
enable them by default merely because the pure Rust oracle passes. Existing legacy
crash tests do not certify this physical format. The new G9 driver is a test suite,
not evidence that its schedules have successfully executed.

## Evidence and review status

The G9 core tests cover independent incarnation-set Boolean oracles, TID reuse,
wrong-generation retirement, duplicate coordinates, compact/unaligned records,
boundary offsets, malformed/truncated bytes, empty/dense groups, nested pruning,
clear-only private replay, cancellation, work counters and a golden wire image.
Logical merge tests cover retired TID reuse, duplicate complete coverage, live
generation conflicts, source identity checks, capacity failure and cancellation.
The grouped kernel tests exhaustively check scalar bit truth tables and ordered
page iteration. CI compiles and executes the Rust tests; source review alone is
not evidence of success. The PR records exact validated commits and CI runs.

Self-reviewed only. Independent storage review remains open. PostgreSQL SQL
row-identity equality across UPDATE, HOT, DELETE, VACUUM, mixed-format compaction,
long snapshots and hard crashes remains required. The newly added PostgreSQL physical crash suite has not run for the adapter. Existing ignored Unicode fixture tests remain a separate CI gate.

After correctness, require selective/common AND, broad OR, warm/cold cache,
concurrent writes, bytes decoded/read, private memory, index size, WAL, build
and VACUUM time, and p95/p99. No TIN-equivalent performance follows merely from
implementing a similar logical bitmap shape.

## Official contracts reviewed

- [TIN architecture](https://planetscale.com/blog/introducing-tin), published
  September 16, 2026, erratum September 20: inspiration for 256-page pruning,
  offset masks and shared liveness, not a complete generation/recovery proof.
- [PG18 index scans](https://www.postgresql.org/docs/18/index-scanning.html):
  matching TIDs are candidates; heap visibility remains PostgreSQL's job.
- [PG18 index locking](https://www.postgresql.org/docs/18/index-locking.html):
  remove all index references before heap deletion; bitmap scans require MVCC.
- [PG18 generic WAL](https://www.postgresql.org/docs/18/generic-wal.html):
  registered copies, exclusive buffer locks, registration order, standard page
  layout and the server's maximum registered-page count remain mandatory.
- Rust 1.98.1 [u64](https://doc.rust-lang.org/1.98.1/std/primitive.u64.html),
  [u32](https://doc.rust-lang.org/1.98.1/std/primitive.u32.html),
  [slices](https://doc.rust-lang.org/1.98.1/std/primitive.slice.html),
  [array construction](https://doc.rust-lang.org/1.98.1/std/array/fn.from_fn.html)
  and [iteration](https://doc.rust-lang.org/1.98.1/std/iter/trait.Iterator.html):
  explicit little-endian fields, bounded shifts, checked extents, set-bit
  traversal and initialized fixed-size arrays. Retrieved stable documentation
  reports version 1.98.1, commit `48a229cea`; CI pins Rust 1.98.1.

The existing PostgreSQL 18.6 and pgrx 0.19.2 immutable boundary references remain
in `docs/api-evidence.md`. No new pgrx/FFI API is used here. No Rust toolchain was
installed in the development VM; executable validation is performed in CI.
