# G7 worker protocol

This document extends the existing selective-merge work. The copying path and
sealed-prefix retention retain their existing publication, deletion and recovery
contracts. No persistent format or shared extent reference counter is introduced.

## Disjoint work

`mutable::work::WorkState` contains eleven checked integer words. A participant
copies them under the host coordination lock, reads and validates one page outside
the lock, and replaces the words only if its original copy still matches. Only
the successful participant consumes that private batch. Failed claims discard
private data. A participant failure fails the query, rather than reassigning a
partially processed batch and risking duplicate output.

Term work includes the inline dictionary owner exactly once, followed by the
captured posting chain. Ordering checks span pages and reject duplicate owner
coordinates or regressing incarnations. A one-term necessary condition can cover
an AND or phrase, but only an exact single-term query can offer a sealed membership
candidate to the existing VM proof. Wider unions and negation partition canonical
owner pages instead. No shared set of matching document IDs is required.

The host retains its shared structural barrier while any captured work remains
reachable. VACUUM can remove canonical liveness concurrently; a consumer must
reread the owner under the existing cleanup-blocking pin before synchronous heap
fetch or count certification. A captured candidate is never a visibility result.
Each scan captures fresh state on restart. Shared words contain no backend address,
allocator-owned object, snapshot pointer, buffer handle or Rust synchronization.

## Maintenance

The restart-only, default-off `pin.enable_parallel_vacuum` enables PostgreSQL's
parallel bulk-delete and cleanup flags. One core worker owns one complete index
per phase. The exact `IndexBulkDeleteResult` is the only statistics object passed
between phases. No private pointer is appended to that record.

Both maintenance phases borrow `IndexVacuumInfo.strategy`. Page reads use
`ReadBufferExtended`; traversal boundaries use `vacuum_delay_point(false)` after
content locks and generic WAL operations have ended. PostgreSQL remains responsible
for worker admission, dead-TID storage, cost accounting and serial fallback.

## Official contracts reviewed

PostgreSQL 18.6 source commit `724edf9bde9d356724ad384a2e196edc3c9f80f7`:
`src/include/access/amapi.h`, `src/include/access/genam.h`,
`src/include/commands/vacuum.h`, `src/backend/commands/vacuumparallel.c` and
`src/backend/access/index/indexam.c`. Manual sections: `index-functions.html`,
`index-locking.html`, `custom-scan-execution.html`, and `sql-vacuum.html`.

Rust 1.98.1 contracts: checked integer conversions and arithmetic, fixed arrays,
fallible Vec reservation in the existing cover builder, and slice validity at the
host boundary. The work implementation contains no unsafe operations and allocates
no per-page result set. Code review is not execution or performance evidence.

## Validation status

`g7_work.rs` supplies competing-claim, coverage, missing-term, restart, liveness,
sealed-certificate and malformed-state tests. Native lifecycle qualification and
independent storage/FFI review remain required before experimental capabilities
are promoted. Benchmarking is outside this implementation update.
