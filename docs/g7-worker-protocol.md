# G7 worker protocol

G7 uses PostgreSQL processes and PostgreSQL DSM. No Rust thread pool, shared Rust
object, backend pointer or persistent shared-work format is introduced.

## Disjoint count work

`mutable::work::WorkState` contains eleven checked `u64` words. A participant
copies those words under the host spinlock, reads and validates one page outside
the spinlock, and publishes the successor only if the original shared words still
match. Only the successful claimant consumes its private batch.

A failed compare-and-replace discards private work and retries from a new shared
snapshot. An ERROR after a successful claim aborts the complete SQL statement.
The host does not reassign that batch inside a partially successful query.

Single-term work includes the inline dictionary owner exactly once, followed by
the captured posting chain. Wider unions and negation use canonical owner pages,
which gives duplicate-free physical coverage without a result-sized shared set.
Ordering checks span posting pages and reject duplicate owner coordinates or
regressing incarnations.

Shared words are coordinates and bounded traversal state only. They contain no
relation pointer, snapshot pointer, buffer handle, allocator pointer, Rust object,
vtable or synchronization primitive.

## Direct count workers

The serial G5 `PinCount` node remains the semantic and visibility reference.
`pin.parallel_count_workers = 0` disables its worker path by default. Enabled
workers are further capped so leader plus workers fit one `work_mem` budget
using a conservative decoded-query, matcher, analyzer, detoast and batch peak.

When enabled, the leader:

1. captures `WorkState` under the structural reader barrier;
2. enters PostgreSQL parallel mode;
3. creates one `ParallelContext`;
4. copies relation OIDs, attribute number, query bytes, work words and aggregate
   counters into DSM;
5. launches workers and participates in the same claim loop;
6. waits for all workers and copies the exact aggregate result;
7. destroys the parallel context before releasing the outer structural barrier.

PostgreSQL restores transaction state, active snapshot and GUCs in parallel
workers. A worker reopens heap and index relations, validates the Pin index,
creates its own heap fetch state and tuple slot, and executes the existing G5
owner-pin and visibility protocol.

The coordinator spinlock covers only fixed word and counter copies. No page read,
query decode, heap fetch, visibility-map lookup, Rust callback or ERROR-capable
operation runs under that spinlock.

## Parallel build workers

Core decides build worker eligibility and writes the requested count to
`IndexInfo.ii_ParallelWorkers`. Pin accepts nonconcurrent parallel build only.

The leader allocates one PostgreSQL parallel table scan in DSM. Workers reopen
the heap with `ShareLock` and the index with `AccessExclusiveLock`, matching
the nonconcurrent build leader. Each participant calls `BuildIndexInfo`,
`table_beginscan_parallel`, and `table_index_build_scan`.

The scan callback preserves PostgreSQL's heap/HOT build semantics. Document
analysis and preparation happen independently in each participant. The existing
Pin writer interlock still serializes generic-WAL publication, so build workers
do not create a new concurrent page-mutation protocol.

Each participant is charged a conservative peak covering document preparation,
the maximum detoasted input and fixed callback state. Pin caps leader plus workers
so this total does not exceed `maintenance_work_mem`.
If DSM allocation or worker launch is unavailable, the leader destroys the
parallel context and runs the established serial build.

## Parallel VACUUM

The restart-only, default-off `pin.enable_parallel_vacuum` advertises
PostgreSQL's parallel bulk-delete and cleanup flags. PostgreSQL assigns one whole
index to one process in a phase. Pin does not partition one VACUUM index scan
internally.

The exact `IndexBulkDeleteResult` representation is the only statistics object
passed between phases. Both phases borrow `IndexVacuumInfo.strategy` and read
pages through `ReadBufferExtended`. Traversal boundaries call
`vacuum_delay_point(false)` only outside content locks and generic-WAL batches.

## Failure and shutdown

`ParallelContext` is registered with PostgreSQL transaction and subtransaction
cleanup. Explicit success paths wait for workers, accumulate results and destroy
the context. Worker ERROR or termination aborts the SQL operation. PostgreSQL
then releases relation locks, DSM and worker resources through its normal error
cleanup.

Test builds expose deterministic worker pause stages:

- stage 16: parallel build callback before Rust document insertion;
- stage 17: direct-count worker before claiming work;
- stages 7 through 12: existing storage/maintenance transition windows.

The disposable qualification harness holds advisory lock `(180006, 4)` only to
stop a test worker at these boundaries. Production builds do not expose the pause
GUC or test event calls.

## Official contracts reviewed

PostgreSQL 18.6 commit `724edf9bde9d356724ad384a2e196edc3c9f80f7`:

- `src/include/access/parallel.h`
- `src/backend/access/transam/parallel.c`
- `src/include/access/tableam.h`
- `src/backend/catalog/index.c`
- `src/backend/access/gin/gininsert.c`
- `src/include/access/amapi.h`
- `src/include/access/genam.h`
- `src/include/commands/vacuum.h`
- `src/backend/commands/vacuumparallel.c`
- `src/backend/executor/nodeCustom.c`

Manual sections reviewed: Index AM functions, CustomScan execution, parallel
safety, index locking and VACUUM.

Rust 1.98.1 contracts reviewed: checked integer conversions, fixed arrays and
`slice::from_raw_parts`. The pure `WorkState` implementation contains no
unsafe operations.

## Validation

Pure tests cover competing claims, duplicate-free coverage, missing terms,
restart, liveness changes, sealed-count certification and malformed shared words.
Host qualification requires observed workers for build and direct count, worker
termination, exact serial equality, subsequent successful reuse and ordinary
parallel bitmap equivalence.

These tests establish correctness evidence only. They do not establish a latency,
throughput, memory-footprint or performance-parity claim.
