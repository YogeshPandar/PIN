# G7 selected implementation and acceptance contract

Status: code implementation under PostgreSQL qualification; no performance or release claim.
Base: ae69a2e0fc2f2c241b7dfbe5a09f592061115351.
PostgreSQL source: 18.6 at 724edf9bde9d356724ad384a2e196edc3c9f80f7.

## Implemented work

1. Keep PostgreSQL's native parallel bitmap heap path as the row-producing
   parallel baseline. Pin builds the bitmap once; PostgreSQL distributes heap
   blocks, visibility and rechecks. `amcanparallel` remains false because Pin
   does not implement the Index AM parallel scan callbacks or `amgettuple`.
2. Add PostgreSQL-worker parallel build. `amcanbuildparallel` is true and the
   build uses the core-requested `IndexInfo.ii_ParallelWorkers`, PostgreSQL
   `ParallelContext`, a parallel table scan and `table_index_build_scan`.
   Expensive text analysis runs in participants while the existing Pin writer
   interlock serializes durable publication.
3. Add an opt-in PostgreSQL-worker direct-count path. The existing narrow
   `PinCount` eligibility and G5 visibility proof are unchanged. Workers claim
   disjoint pointer-free work batches through fixed DSM words, then perform the
   same owner, visibility-map and heap fallback checks as serial PinCount.
4. Add restart-only, default-off parallel VACUUM capability advertisement.
   PostgreSQL owns worker launch, DSM, dead TID storage, cost delay and serial
   fallback. Pin uses the callback-owned buffer strategy and preserves the exact
   `IndexBulkDeleteResult` ABI.
5. Add optional sealed-prefix ownership retention to the copying compactor.
   Canonical owners, term identity, liveness, WAL publication and recovery
   remain unchanged. The copying implementation remains the default reference.

## Parallel build protocol

Core first decides whether parallel build is eligible and stores the requested
worker count in `ii_ParallelWorkers`. Pin rejects concurrent build parallelism
and falls back to the existing serial build if no worker was requested, DSM
cannot be created, or no worker launches.

The leader enters PostgreSQL parallel mode, creates one `ParallelContext`, and
places only relation OIDs, the fixed participant memory budget, aggregate
statistics and a `ParallelTableScanDesc` in shared memory. Workers reopen the
heap and index with the same nonconcurrent build lock modes and validate the Pin
storage relation before scanning.

Every participant calls `table_index_build_scan` against the same PostgreSQL
parallel table scan. PostgreSQL keeps heap-build and HOT-root behavior authoritative.
The callback analyzes one document outside Pin's writer interlock, then reuses the
existing WAL-backed insertion path. Publication remains serialized, so parallelism
does not introduce a second durable storage protocol.

Pin computes a conservative participant peak from the prepared-document budget,
the maximum detoasted input and fixed callback state. The requested worker count
is capped so leader plus workers cannot exceed `maintenance_work_mem` under that
peak. `amusemaintenanceworkmem` is therefore true. The leader accumulates worker
buffer/WAL instrumentation and exact build statistics after workers finish.

## Parallel direct-count protocol

`pin.parallel_count_workers` defaults to zero. `pin.enable_count_fastpath`
also remains off by default. Both must be enabled before PinCount attempts its
internal worker path. The number of requested workers is capped by `max_parallel_workers_per_gather`
and by one total `work_mem` budget. The per-participant ceiling includes the
decoded query, matching scratch, analyzer budget, maximum detoasted input and
fixed batch state.

The leader captures one duplicate-free `WorkState` while holding the structural
reader barrier. DSM contains only checked integer work words, relation OIDs,
attribute number, immutable query bytes, aggregate counters and a PostgreSQL
spinlock. It contains no Rust allocation, buffer pointer, snapshot pointer,
relation pointer or backend-local address.

A participant copies the work words while holding the spinlock, validates and
prepares one private batch outside the spinlock, then atomically claims the
successor only if the shared words still match its original snapshot. Only the
successful claimant consumes that batch. This prevents double counting without
a result-sized shared set.

Workers reopen relations locally. PostgreSQL parallel startup restores the active
transaction snapshot and GUC state. Each worker then runs the same G5 owner pin,
fresh visibility-map read and HOT-aware heap fallback protocol. Any participant
error aborts the query; claimed work is never silently retried. A subsequent
execution captures fresh work from the index.

## Parallel VACUUM protocol

`pin.enable_parallel_vacuum` is a postmaster-start, default-off control. When
enabled, Pin advertises PostgreSQL's parallel bulk-delete and cleanup options.
One PostgreSQL process owns a complete index in each phase. Pin does not split
one index internally during VACUUM.

Both phases borrow `IndexVacuumInfo.strategy` for index page reads. Traversal
boundaries call `vacuum_delay_point(false)` only after content locks and generic
WAL batches end. PostgreSQL remains responsible for worker memory adjustment,
shared cost balance, dead-item DSM and serial fallback.

## Prefix-retention protocol

The host holds the existing exclusive structural barrier before the writer
interlock through inspection, publication and retirement. No old reader can
retain an affected posting chain. The optimization does not introduce concurrent
shared extent ownership or a reference-count format.

Only a contiguous, completely live sealed prefix can remain in place. The final
retained page is the publication boundary and its old suffix is rewritten. Pin
publishes the metapage, dictionary and boundary update in the existing bounded
generic-WAL operation. Only the detached suffix enters the retirement journal.
Recovery therefore cannot reclaim a retained page.

The default `CompactMode::Copy` remains the differential oracle.
`pin.enable_compact_reuse` is privileged and defaults off.

## Acceptance evidence

The code-backed G7 qualification must establish:

- actual PostgreSQL workers for build, PinCount and native parallel bitmap plans,
  not merely planned worker counts;
- serial and parallel result equality, including lossy bitmap rechecks;
- exact PinCount equality before and after worker termination;
- prepared execution and own-write fallback correctness;
- build correctness using the same PostgreSQL heap build semantics;
- cancellation, worker termination and shutdown cleanup;
- no duplicate work claims under competing participants;
- bounded parallel build memory under one `maintenance_work_mem` budget;
- copying and retained-prefix compaction equivalence;
- publication, restart and deletion recovery without reachable reclaimed pages.

Controlled throughput, latency and total-memory measurements remain separate
performance evidence and are not asserted by this document.

## Gated dependencies

A PostgreSQL ranked CustomScan is not present in the current G4 implementation.
G4 provides bitmap query execution only. G7 therefore does not advertise a
parallel ranked CustomScan or parallel top-k until a correct serial PostgreSQL
ranked executor, score binding and eligibility contract exist. Setting a parallel
flag without that serial contract would advertise behavior the AM cannot honor.

General multi-owner shared-payload/reference-count merging and Pin index reads
during hot standby recovery also remain gated. The implemented retained-prefix
path is ownership retention under reader quiescence, not general concurrent
payload sharing.

## Official contracts

- PostgreSQL 18 Index AM functions: https://www.postgresql.org/docs/18/index-functions.html
- PostgreSQL 18 CustomScan execution: https://www.postgresql.org/docs/18/custom-scan-execution.html
- PostgreSQL 18 parallel safety: https://www.postgresql.org/docs/18/parallel-safety.html
- PostgreSQL 18 generic WAL: https://www.postgresql.org/docs/18/generic-wal.html
- PostgreSQL source pin: 724edf9bde9d356724ad384a2e196edc3c9f80f7
- `src/backend/access/transam/parallel.c`
- `src/include/access/parallel.h`
- `src/include/access/tableam.h`
- `src/backend/access/gin/gininsert.c`
- `src/backend/commands/vacuumparallel.c`
- Rust 1.98.1 slice contract: https://doc.rust-lang.org/1.98.1/std/slice/fn.from_raw_parts.html
- Rust 1.98.1 checked integer conversion: https://doc.rust-lang.org/1.98.1/std/primitive.usize.html

Independent storage and FFI review remains required before experimental controls
are promoted from their default-off state.
