# G5 visibility-aware direct counts

G5 is an experimental exact-count optimization built on the merged G3 storage
protocol and G4 streaming query executor. Both public switches default to off.
This document records the implemented proof obligations and observed evidence.
It is not a production-readiness or performance claim.

## Scope

The custom upper path handles only plain `COUNT(*)` over one ordinary heap
relation with one constant exact single-term Pin predicate and no residual
restriction. The planner decodes that bounded typed constant once and declines
phrases, Boolean expressions, prefix queries and other compound shapes. It excludes RLS,
security quals, inheritance, lateral dependencies, joins, grouping, DISTINCT,
aggregate FILTER or ORDER BY, row marks, LIMIT/OFFSET, CTEs, recovery and
SERIALIZABLE execution. Partial and expression indexes are excluded.

The candidate stream is duplicate-free and bounded. A single exact term may
mark candidates from sealed posting pages as certification candidates. The
inline dictionary owner and every mutable or compound-query candidate remain
heap checked. Exact predicate evaluation is retained for heap fallback, so
candidate membership is never treated as the final SQL answer.

The ordinary bitmap path remains available. No `amgettuple`, index-only return,
parallel count, standby scan, Boolean VM certification or performance result is
advertised by this work.

## PostgreSQL contracts

Implementation evidence is pinned to PostgreSQL 18.6 commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7`.

`visibilitymap_get_status` explicitly permits a stale VM observation and makes
the caller responsible for concurrency. PostgreSQL's index-only executor relies
on ordering between VM clearing, index publication and MVCC snapshot acquisition.
Pin therefore does not cache an all-visible result across candidates. Every
certification attempt calls PostgreSQL's VM accessor while the canonical owner
page remains protected by a buffer pin.

PostgreSQL's buffer-manager contract requires cleanup operations to hold an
exclusive content lock and observe that the shared pin count is one.
`LockBufferForCleanup` implements this by waiting for other pins to drain.
Pin uses that exact interlock before publishing an owner removal.

`table_index_fetch_tuple` is the heap fallback because it follows a HOT chain
from the indexed root under the executor snapshot and may update a private TID
copy to the visible version. Pin never replaces the posting identity with that
visible TID.

Relevant upstream files:

- `src/backend/executor/nodeIndexonlyscan.c`
- `src/backend/access/heap/visibilitymap.c`
- `src/backend/storage/buffer/README`
- `src/include/access/tableam.h`
- `src/backend/optimizer/util/pathnode.c`
- `src/backend/optimizer/plan/createplan.c`
- `src/backend/optimizer/plan/setrefs.c`
- `src/backend/executor/nodeCustom.c`

## Owner and VACUUM protocol

For one owner-page batch the executor performs this sequence:

1. Acquire the canonical owner buffer and copy it under a shared content lock.
2. Release the content lock but retain the ResourceOwner-backed buffer pin.
3. Validate the candidate incarnation, publication state and liveness from the
   copied owner page.
4. For a sealed exact-term candidate, obtain fresh VM status. If the page is
   all-visible and VM certification is enabled, count the candidate directly.
5. Otherwise call `table_index_fetch_tuple` under the active MVCC snapshot and
   exactly re-evaluate the document predicate.
6. Release the owner pin after every VM and heap decision in the batch completes.

VACUUM reads and mutates owner state under the existing writer interlock. When an
owner page changes, `PageStore::remove_owners` is a separate required operation.
The PostgreSQL adapter calls `LockBufferForCleanup` before registering the page
with generic WAL. The default PageStore implementation returns an error instead
of silently falling back to an ordinary commit.

This means a reader cannot release the owner protection before a heap fallback
and race with removal plus physical slot reuse. It also means VACUUM cannot
publish a removal while a count still relies on the copied owner state.

The count path never holds an index content lock while fetching the heap or
waiting at a deterministic test pause. At most one canonical owner page remains
pinned at a time. Normal completion releases it explicitly. PostgreSQL
ResourceOwner cleanup handles ERROR, cancellation and backend death.

## VM gate

Two superuser-settable GUCs control the experiment:

```sql
SET pin.enable_count_fastpath = on;
SET pin.enable_count_vm = on;
```

Both default to `off`. With the fast path enabled and VM certification disabled,
the custom node still performs snapshot-visible heap checks. Enabling VM only
allows sealed exact-term candidates that pass the owner checks to use the
all-visible shortcut. Mutable and uncertified candidates still fetch the heap.

VM status is never persisted in Rust state. A VM buffer may remain pinned for
PostgreSQL's normal buffer reuse optimization, but its status is reread for each
candidate.

## Planner and cached-plan contract

The upper hook keeps a real `AggPath` child as runtime fallback. PostgreSQL's
`add_path` may immediately free a dominated non-index path, so Pin makes a
shallow planner-owned copy before adding its CustomPath. The candidate is tied
to the same BitmapHeapPath and Pin IndexPath selected by the core aggregate.

Costing starts from the full eligible core aggregate cost and credits only the
omitted aggregate transition plus a small fixed setup charge. It does not assume
a visibility-map hit rate or assign an artificial zero cost.

The plan stores only PostgreSQL Nodes and identifiers. It records the private
index OID in the planner relation dependency list. Execution reopens and checks
the heap/index relationship, key attribute, text type, collation, operator family,
index validity/readiness/liveness and `indcheckxmin` before reading Pin storage.
Cached plans fall back to the retained core aggregate when the count switch,
isolation, snapshot, recovery or heap/RLS eligibility changes.

## Bounded work

The pure candidate iterator uses fixed state and does not collect the complete
result set in a Vec, HashSet, BTreeSet or TIDBitmap. The host batch contains at
most 64 candidate records. Counters use checked arithmetic. Heap fallback
analyzes one visible document at a time and resets its PostgreSQL scratch context
before the next candidate.

The implementation is intentionally conservative. Complex predicates remain on
PostgreSQL's ordinary bitmap/heap aggregate path. VM certification is limited to
exact sealed single-term membership.

## Deterministic qualification

`tools/g5_visibility_model.py` models the owner pin, copied liveness, VM read,
count decision, cancellation, cleanup, heap removal and slot reuse as explicit
transitions. Snapshot visibility and the reclamation horizon are supplied
PostgreSQL facts rather than reimplemented MVCC.

The model has six correct cases:

- deletion before the reader snapshot
- retained old snapshot
- owner removed before reader acquisition
- failed publication
- uncommitted sealed owner
- mutable source

Four deliberately broken modes must produce replayable counterexamples:

- ignore the owner pin
- copy liveness before pin acquisition
- use detached liveness
- cache an old VM result

The frozen model graph contains 171 states and 249 transitions across the six
correct cases. These numbers are model coverage, not PostgreSQL execution states.

The test-hooks build adds stages 13 through 15 for owner-pin acquisition, the
pre-visibility window and the post-visibility window. The pauses use advisory
locks only after index content locks are released. The host qualification suite
uses those points to check an old snapshot, VACUUM waiting on a BufferPin,
cancellation, backend termination and crash during incomplete publication.

## Evidence and remaining gate

Local Python/model checks can validate the state machine and source contracts
without installing Rust. Rust formatting, compilation, Clippy, rustdoc, C
compilation, PostgreSQL SQL schedules and crash tests remain CI/host evidence.

G5 is not complete until real PostgreSQL execution confirms count equality under
old snapshots, deletion and reuse, HOT/non-HOT updates, compaction, failed
publication, VM transitions and cancellation. Independent visibility and unsafe
review also remains required. Performance and bare-PostgreSQL parity are
unmeasured, so both GUCs stay off by default.
