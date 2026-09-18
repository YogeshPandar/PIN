# G3 sealed posting compaction

Status: implementation in progress; this checkpoint is not qualified or merge-ready.
Base: `c5a1c6ca7c95656de517533c534a9d2c5f852679` (merged G2).

## Scope

Implement compressed immutable posting payloads, a mutable append tail, bounded-scratch
copy-on-write compaction, durable replacement/reclamation, and PostgreSQL reader
quiescence. Canonical owner slots remain stable. Every candidate still requires
PostgreSQL heap visibility and exact predicate rechecks. No ranking, direct counts,
synchronous tuple scans, background workers, or standby reads are enabled.

The available snapshot assigns structural compaction to G3. The original standalone
technical blueprint was not available in this session; this document records the
explicit implementation scope rather than silently claiming every original G3 gate.

## Protocol

- Retain one canonical incarnation-qualified owner and liveness bit per document.
  Sealed postings reference that owner; compaction never creates a second liveness copy.
- Stream ordered references into independently decodable compressed pages. Preserve
  full owner incarnation identity. Use checked arithmetic and reject malformed bytes.
- Persist every unreachable output page together with its metapage recovery journal.
  The dictionary continues to point to the old complete chain during preparation.
- Publish the replacement dictionary head/tail and the retired-chain journal in one
  Generic WAL record. Then reclaim retired pages in restartable atomic batches.
- Bitmap scans acquire a shared structural barrier before capturing page references.
  Maintenance acquires its exclusive side before the existing writer interlock.
  No reader can retain a reclaimed structural page. PostgreSQL owns ERROR cleanup.
- Normal inserts append to mutable pages, never to sealed posting payloads. A sealed
  tail can gain a successor link. Reclaimed pages can serve new sealed or fragment data.

This is a serialized maintenance baseline, not nonblocking/background compaction.
Owner/dictionary reclamation and relation truncation remain separate obligations.

## Official contracts

- PostgreSQL 18 index locking: https://www.postgresql.org/docs/18/index-locking.html
- PostgreSQL 18 candidate/recheck rules: https://www.postgresql.org/docs/18/index-scanning.html
- PostgreSQL 18 Generic WAL: https://www.postgresql.org/docs/18/generic-wal.html
- Pinned `LockPage` implementation: https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/storage/lmgr/lmgr.c
- Rust checked integer operations: https://doc.rust-lang.org/std/primitive.u64.html

## Acceptance gates

Checked-codec corruption and boundary tests; mixed mutable/sealed scan equivalence;
VACUUM/slot-reuse correctness; idempotent recovery at every durable transition;
bounded scratch and finite traversal; sequential/bitmap SQL equality; reader barrier
and abort cleanup; actual postmaster crash/restart tests. Rust and PostgreSQL tests
must run in CI, not an installed Rust toolchain in the editing VM. Independent unsafe
review and measured performance remain explicit obligations.
