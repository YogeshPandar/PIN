# Issue 14: selective grouped scans and attributable measurements

Baseline: `1d80b58e0eb17b003326283f8050ac3556f80776`, PostgreSQL 18.6.
This work addresses [issue 14](https://github.com/YogeshPandar/PIN/issues/14).
It does not close the issue or establish a 10x result.

## Evidence and implementation direction

The issue's small warm fixture shows fewer page visits for broad grouped scans,
but worse sparse-term throughput and an unbounded recheck candidate set as the
write delta grows. Those observations are not isolated CPU measurements.

Source inspection identifies avoidable work in the current implementation:

- The snapshot enumerator chooses an AND cover by the number of query terms.
  Equal-size covers follow query order, not the next possible matching group.
- Every positive group performs a fresh liveness catalog lookup, discarding the
  previous leaf, even though output advances monotonically in heap order.
- Bitmap demand walks every candidate page and every query node before the
  actual offset evaluator walks those pages again.
- Single-term queries allocate grouped scratch and consult the supplemental
  catalog even when the canonical posting fits inline or in one small page.
- Offset masks are assembled one byte at a time rather than one little-endian
  machine word at a time.

The implementation will use monotone Boolean lower bounds for group seeks,
reuse the liveness cursor, propagate demanded page masks backwards once, and
retain a bounded canonical sparse path. No disk-format change, borrowed host
buffer, visibility shortcut or new unsafe operation is needed for these changes.
Small-posting selection is a cost hypothesis, not a universal speed guarantee.

## Contracts and primary references

PostgreSQL's [index scanning contract](https://www.postgresql.org/docs/18/index-scanning.html)
requires complete candidate coverage and exact membership whenever recheck is
false. Heap visibility remains PostgreSQL's responsibility. Bitmap counts are
candidate accounting, not visible SQL counts.

The pinned [GIN scan implementation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/gin/ginget.c)
separates scan-key consistency, advancing posting streams and bitmap emission.
The pinned [TIDBitmap implementation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/nodes/tidbitmap.c)
retains per-page recheck/lossiness semantics. This work must not substitute an
index-only timer or popcount for complete query latency or MVCC-visible count.

Rust's [`u64::from_le_bytes`](https://doc.rust-lang.org/std/primitive.u64.html#method.from_le_bytes)
and [`slice::as_chunks`](https://doc.rust-lang.org/std/primitive.slice.html#method.as_chunks)
provide alignment-independent word decoding with an explicit bounded tail.
The workspace toolchain and Cargo-generated lockfile remain unchanged.

PlanetScale's [TIN introduction](https://planetscale.com/blog/introducing-tin)
and [search-engine anatomy](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine)
are design references for avoiding postings work, not specifications or measured
Pin results. TIN's published ranked benchmark is not an equivalent GIN BM25 test.

## Qualification plan

Compare the new group enumeration with the independent document oracle for
nested AND/OR/NOT, missing terms, both operand orders, reused heap TIDs and
post-snapshot writes. Add deterministic page-read bounds to expose catalog work
that scales with common rather than matching groups. Check all offset widths and
compare mask demand against the previous page-by-page algorithm.

Run existing normal/fault/recovery qualification and the pinned Rust CI matrix.
Do not install Rust in the development VM. Keep both grouped feature gates off
by default. Compilation, tests and performance must be reported separately.

Add a paired G9/legacy/GIN measurement path that verifies row identities and
actual bitmap plans, captures repeated latency samples and plans, and separates
backend CPU from client time. Unavailable hardware counters, allocation profiles
or cold-cache evidence must remain explicitly unavailable, not zero.

## Remaining architectural gates

The owner-ordered delta still emits newer live owners with recheck required.
Filtering it efficiently needs a term-addressable append frontier or another
measured delta design, not an unsafe subtraction of global term sets. Phrase and
prefix qualification, full-rebuild writer stalls, sustained-write tails, large
corpora, true cold cache, ranking and MVCC-certified aggregate acceleration remain
separate evidence requirements. Do not enable a new path by default or close the
issue merely because structural work counts improve.
