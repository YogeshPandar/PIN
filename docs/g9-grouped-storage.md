# G9 generation-safe grouped storage

Status: implementation in progress; no SQL activation or performance claim.
Base: `ffc48f54d8a02996bba9405bb3c743dcb2db0fc6`.

## Identity comes before bit operations

A segment contains complete documents, not arbitrary independent term flushes.
Within one relation generation and segment identity, each heap root coordinate
maps to exactly one canonical owner incarnation. The mapping is immutable.
Different occupants of a reused coordinate must belong to different segments.
Boolean expressions are evaluated inside that identity boundary; only completed
matches may be unioned across segments. Intersecting term masks across segment
identities is forbidden. Merging cannot silently collapse different owners at
the same coordinate. Retain separate segments or prove the old owner retired.

The new logical format is separate from existing mutable page tags. Legacy
mutable, sealed and tag-9 direct chains remain readable and remain the SQL path.
A logical 256-page group can exceed one PostgreSQL page. It must not be stuffed
into an 8 KiB page or confused with a WAL-atomic extent. Physical extent pages,
manifest publication, migration and recovery must be implemented together.

## Foundation

Implement checked little-endian logical group records with a version, relation
and segment generation identity, aligned heap-page base, 256-bit page mask,
a compact per-present-page directory, and variable-length offset bitmaps.
Keep the group directory separate from offset decoding. Scalar evaluation must
prune page masks before asking for offset payloads and must not allocate.
Use caller-owned output and bounded stack scratch. The heap offset domain comes
from `HeapLayout`, not a hardcoded assumption about 291 or 512 heap tuples.

Store canonical owner/incarnation membership once for the segment group.
Liveness is shared by every term in that group and can only clear, never set,
for an existing segment. A retirement request must match both the coordinate
and incarnation. This is index retirement, not snapshot visibility.

AND intersects candidate pages. OR unions candidate pages. Difference must
retain the left page mask: sharing a page does not imply sharing every offset.
All operations intersect their tuple results with shared liveness. A missing
term is empty; NOT needs an explicit complete segment universe, not all bits.

## PostgreSQL integration gates

1. Seal complete owner coverage under the existing structural and writer
   barriers; create immutable generation mapping and shared liveness first.
2. Write bounded extent batches, WAL-log an unreachable build journal, then
   atomically publish manifest coverage. Keep old coverage until that switch.
3. VACUUM must retire canonical owners and every published segment's shared
   liveness before `ambulkdelete` returns and permits heap slot reuse. A failed
   or interrupted pass must be replayable and idempotent.
4. Readers capture a manifest under a structural barrier; liveness reads need
   the matching buffer protocol. No borrowed shared-buffer pointer survives
   unlock. Reclaim source extents only after readers quiesce.
5. Preserve immediately searchable mutable writes and owner-aware fallback for
   mixed storage. Evaluate full documents within a segment; do not AND the
   partial results of independent per-term migration streams.
6. Add a default-off SQL setting only after the path actually exists. Preserve
   PostgreSQL heap visibility checks, bitmap recheck flags, MVCC-only scans,
   interruption, memory budgets and the generic WAL registration bound.

## Evidence required

Use an independent owner-set oracle, duplicate/reused TIDs, different segment
identities, empty/dense/sparse groups, boundary offsets, malformed/truncated
records, cancellation, and clear-only retirement/replay tests. Then require
SQL row-identity equality with the legacy path across UPDATE, HOT, DELETE,
VACUUM, compaction, long snapshots and crash recovery. Existing legacy crash
tests do not certify the new physical format.

Benchmark only after correctness: selective/common AND, broad OR, warm/cold
cache, concurrent writes, bytes decoded/read, private memory, index size, WAL,
build and VACUUM time, and p95/p99. SIMD, exact-count shortcuts and ranked top-k
remain separate gates. No claim of TIN-equivalent performance follows from
implementing a similar bitmap shape.

## Official contracts reviewed

- [TIN architecture](https://planetscale.com/blog/introducing-tin): 256-page
  pruning, offset masks and shared liveness are design inspiration, not a full
  publication, generation or recovery specification.
- [PG18 index scans](https://www.postgresql.org/docs/18/index-scanning.html):
  matching TIDs are candidates; heap visibility remains PostgreSQL's job.
- [PG18 index locking](https://www.postgresql.org/docs/18/index-locking.html):
  remove all index references before heap deletion; bitmap scans require MVCC.
- [PG18 generic WAL](https://www.postgresql.org/docs/18/generic-wal.html):
  modify registered copies under exclusive buffer locks; preserve registration
  order, standard page layout and the server's maximum registered-page count.
- [Rust u64](https://doc.rust-lang.org/std/primitive.u64.html),
  [u32](https://doc.rust-lang.org/std/primitive.u32.html) and
  [slices](https://doc.rust-lang.org/std/primitive.slice.html): checked extents,
  explicit little-endian fields, bounded shifts and set-bit iteration. The
  retrieved stable documentation identifies Rust 1.98.1, commit `48a229cea`.

No Rust toolchain is installed in the development VM. Rust compilation,
clippy, formatting and executable tests must be observed in CI, not inferred
from a source review. Independent review of PostgreSQL integration is pending.
