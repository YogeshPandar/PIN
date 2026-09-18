# G2 durable bitmap baseline

Status: implementation in progress; not an accepted G2 gate or a performance claim.
The branch starts at `8d3885ae32a89321f3dba99cdcd9c990b77a0381`.

## Candidate contract

`candidate::CandidatePlan` borrows a validated G1 query. It produces a necessary
condition, never an exact SQL result: a term uses its postings, a phrase uses one
required term, AND chooses one necessary cover, and OR unions both covers.
Negation and prefix queries conservatively use the complete indexed non-null
universe. Empty documents belong to that universe. Every bitmap tuple must carry
`recheck = true`; PostgreSQL performs heap visibility and exact SQL evaluation.
Repeated anchors are deduplicated without copying term strings. Scratch is bounded
by the query size and an explicit capacity-accounted memory budget. Probe count is
only a heuristic, not measured selectivity. No result-sized Rust set is allocated.

## Evidence and obligations

- PostgreSQL 18.6 source commit: `724edf9bde9d356724ad384a2e196edc3c9f80f7`.
- pgrx 0.19.2 source commit: `70383e884582d1bcc7cd681d10886b995a2830cb`.
- Rust 1.98.1 remains the locked compiler; no Rust installation in the editing VM.
- https://www.postgresql.org/docs/18/index-scanning.html: all true matches must
  be returned; lossy candidates require rechecks; scan keys are implicitly ANDed.
- https://www.postgresql.org/docs/18/index-locking.html: bitmap scans use the
  core MVCC contract; this does not qualify a synchronous `amgettuple` path.
- https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve_exact:
  reserve fallibly and account actual capacity, not only element count.
- https://doc.rust-lang.org/std/primitive.slice.html#method.sort_unstable:
  in-place sorting of borrowed term bytes; no extra sorting allocation.
- `g2_candidates.rs` checks conservative coverage against the independent G1
  oracle, negation, phrases, repeated anchors, empty documents and budget failure.

The G2 CI qualification now exercises the host boundary rather than only the pure
store model. It forces sequential and bitmap plans and compares exact row identities
for own writes, savepoint rollback, aborted inserts, speculative `ON CONFLICT`,
HOT-eligible updates, indexed-column updates, VACUUM, and physical slot reuse.
A repeatable-read reader is held while independent writer transactions commit, so
newly published index candidates must still be rejected by PostgreSQL visibility.

The test-hooks build also pauses each insertion publication stage after its durable
write and performs an immediate postmaster stop. Recovery restarts the same cluster,
checks bitmap-versus-sequential equality, runs index-cleanup VACUUM, and verifies that
the aborted document never becomes a visible match. A normal fast restart is checked
before the crash matrix as a separate persistence path.

Relevant PostgreSQL 18 contracts:
- https://www.postgresql.org/docs/18/index-scanning.html for bitmap candidates and rechecks.
- https://www.postgresql.org/docs/18/index-locking.html for MVCC index/VACUUM interaction.
- https://www.postgresql.org/docs/18/generic-wal.html for registered page-image mutation.
- https://www.postgresql.org/docs/18/routine-vacuuming.html for index cleanup and reuse.
- https://www.postgresql.org/docs/18/app-pg-ctl.html for fast and immediate server stops.

These fixtures are acceptance evidence only after the matching CI run passes. The
storage implementation still keeps synchronous scans, VM counts, compaction, ranking,
parallel execution, and standby-time Pin index reads gated.
