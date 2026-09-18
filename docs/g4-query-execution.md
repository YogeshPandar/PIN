# G4 streaming query execution

Status: implementation in progress; this checkpoint does not claim test success.
Base: `03cb582b66eb35806c5c1f8ce2b4ade536850851` (merged G3).

## Scope

Implement bounded-memory posting intersections and deduplicated unions over the G3
mutable/sealed chains, with PostgreSQL bitmap integration and independent oracle
coverage. Phrase candidates intersect their required terms, but positions remain a
heap recheck. Negation and prefixes remain conservative universe operands. Never
subtract an approximate set. Keep the existing term-cover scanner as a bounded
fallback when the query needs more resident cursors than its memory budget permits.

The standalone technical blueprint and the ZIP mentioned in the request were not
available in this session. G3's repository documentation also records that absence.
This file defines the implemented G4 work explicitly; it does not assert completion
of unknown original roadmap requirements.

## Invariants

- Compare stable owner page/slot coordinates and verify incarnation identity.
- Validate ascending owners across dictionary-first, mutable and sealed postings.
- Keep a private encoded page and integer decoder state per active term occurrence;
  retain no self-referential slice, PostgreSQL pointer or result-sized Rust set.
- Capture posting tails while holding G3's shared structural barrier. Bound every
  traversal and check host cancellation. Compaction retains structural exclusion.
- Resolve canonical publication/liveness before emitting root TIDs. Always request
  PostgreSQL heap visibility and exact predicate rechecks.
- Budget actual vector capacities. Budget fallback changes work, never query meaning.
- Do not change the on-disk format, WAL, lock order, Cargo.lock or compiler pins.

## Official contracts

- PostgreSQL 18 index scanning: https://www.postgresql.org/docs/18/index-scanning.html
- PostgreSQL 18 index locking: https://www.postgresql.org/docs/18/index-locking.html
- PostgreSQL 18.6 pinned AM implementation:
  https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/index/indexam.c
- Rust fallible capacity reservation:
  https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve_exact
- Rust checked slices: https://doc.rust-lang.org/std/primitive.slice.html#method.get

## Qualification gates

Pure oracle coverage for Boolean/phrase/prefix/negative queries, duplicate clauses,
empty documents, mixed mutable/sealed chains, deletion, slot reuse, malformed owner
ordering, bounded fallback, cancellation, and I/O counters. PostgreSQL sequential
versus bitmap equality must run in the pinned CI environment. No Rust toolchain is
installed in the editing VM. Independent review and measured performance remain
release gates; no bare-PostgreSQL performance parity is claimed.
