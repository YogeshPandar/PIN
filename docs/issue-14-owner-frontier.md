# One-pass dense owner frontier

Status: default-off implementation and qualification candidate for issue #14.
The implementation is not a measured performance result, a production release,
or evidence of parity with PlanetScale TIN. The term-addressed frontier remains
the default and the correctness fallback.

## Problem statement

The grouped snapshot removes most immutable posting work, and persisted frontier
anchors can remove historical posting-prefix traversal. A large related write
delta can still make one Boolean query walk one canonical posting suffix per
changed query term. The September 24 paired CPU evidence records this unresolved
case: after 4,096 related writes, selective AND used 2,880.9 microseconds of PIN
backend CPU versus 1,312.9 microseconds for GIN. That evidence motivates this
experiment. It does not prove which individual function dominates the cost.

The owner frontier tests a narrower hypothesis: when several query terms changed
in the same large delta, it can be cheaper to inspect each new document once
than to replay several term posting streams and join their owner references.
The implementation reuses the canonical owner payload already written by PIN.
It adds no write-time representation, page kind, migration, or WAL record.

PlanetScale's published [TIN architecture](https://planetscale.com/blog/introducing-tin)
is a layout and work-elimination reference. PostgreSQL 18 remains the authority
for index candidates, locking, heap visibility, VACUUM, and recovery behavior.

## Execution boundary

The path is eligible only when all of the following conditions hold:

1. `pin.enable_grouped_scan` and `pin.enable_owner_frontier` are both on.
2. A complete grouped snapshot exists and the newer reserved-incarnation span is
   at least 512.
3. The query needs the complete newer-owner universe, or at least two captured
   query terms have posting tails at or beyond the snapshot fence.
4. The operation's memory budget can retain the complete output root-TID list
   and any one fragmented owner payload.

The 512-incarnation threshold is an unmeasured safety gate. It prevents a new
fixed-cost path from replacing the existing sparse frontier for short deltas.
It must be accepted, changed, or removed using paired measurements rather than
intuition.

For an eligible scan, PIN performs these steps:

1. Capture the grouped snapshot and query-term posting tails under the existing
   structural read protocol.
2. Probe only the captured term tails needed to decide whether the delta is
   sufficiently dense for owner-native evaluation.
3. Walk canonical owner records strictly after the snapshot owner fence.
4. Ignore owners that are unpublished or not live.
5. Parse each complete prepared-document term envelope once and build a query
   membership mask. Boolean membership does not decode unused position deltas.
6. Evaluate the already compiled Boolean program against that mask.
7. Buffer matching root TIDs until the owner walk completes.
8. Emit exact candidates with `recheck=false` only after successful completion.

If capacity is insufficient for the output or a fragmented document, the path
returns before emitting any root TID. The existing term-addressed frontier then
runs from the same captured state. This fail-before-emit rule prevents duplicate
or partial bitmap output during a memory fallback.

## Work model

Let `D` be reserved incarnations after the snapshot, `L` the published live
owners among them, `Q` the changed query terms, `Pq` the posting references
visited for term `q`, and `B` the owner payload bytes read.

The term-addressed path performs approximately:

```text
sum(Pq for q in Q) + owner resolution for Boolean candidates
```

The owner-native path performs approximately:

```text
term-tail probes + D owner slots + B payload bytes + buffered bitmap emission
```

This path should lose for rare or short deltas and can win only when repeated
posting traversal and joining cost more than one owner pass. Fragmented large
documents can also make it lose. The gate therefore remains default off, and
qualification must report gains and regressions by query class and delta size.

## Correctness invariants

The grouped snapshot fence is an index publication boundary, not a PostgreSQL
visibility decision. The owner scan may return candidates inserted after the
statement snapshot. PostgreSQL's bitmap heap scan still applies MVCC visibility,
HOT-chain handling, lossy-page rechecks, other scan keys, and executor quals.
The implementation does not claim an index-only count path.

Every candidate retains the full owner reference and original root TID. Owner
incarnation ordering is checked while walking the chain. A reused heap slot from
another incarnation cannot satisfy a conjunction by combining old and new term
memberships. VACUUM remains responsible for retiring owner liveness before heap
slot reuse is safe.

Only `Published` and live owners can be emitted. Inline payloads and fragmented
payload chains are checked against the owner reference, expected offsets, total
byte count, profile ID, token count, term count, UTF-8 term ordering, positive
position counts, and complete envelope consumption. Position delta values are
not decoded because they cannot change Boolean term presence. Phrase and other
position-dependent shapes keep their established fallback.

The existing shared structural barrier prevents grouped sources and canonical
owner pages from being reclaimed during the scan. The path introduces no new
PostgreSQL pointer lifetime, unsafe block, or borrowed buffer lease. Rust owns
all output and fragmented-payload scratch storage.

Cancellation or corruption returns an error through the existing host guard.
The PostgreSQL bitmap must be discarded on error. The dedicated stage 41 hook
proves selection and permits cancellation/concurrent-write tests without
changing release behavior.

These obligations follow PostgreSQL's official [index scan](https://www.postgresql.org/docs/18/index-scanning.html),
[index AM](https://www.postgresql.org/docs/18/index-functions.html),
[index locking](https://www.postgresql.org/docs/18/index-locking.html), and
[bitmap scan](https://www.postgresql.org/docs/18/indexes-bitmap-scans.html)
contracts. The detailed source and Rust API review is recorded in
[API evidence](api-evidence.md).

## Memory and allocation behavior

The path reserves a fixed 128 KiB budget before variable storage. It then
fallibly reserves at most one `RootTid` per post-snapshot reserved incarnation.
A fragmented document reuses one scratch vector whose capacity cannot exceed
the remaining operation budget. Inline documents do not allocate per owner.
Query term sorting uses a fixed 64-byte index array.

The allocator may return more capacity than requested. The implementation checks
actual `Vec::capacity()` against the operation budget before scanning. This is a
bounded allocation path in `pin-core`; it does not change the allocation-free
`no_std` contract of `pin-kernels`.

## Activation

The optimization is session-local, SUSET, and off by default:

```sql
SET pin.enable_grouped_scan = on;
SET pin.enable_owner_frontier = on;
```

Turning it on does not build grouped storage or frontier anchors. A grouped
snapshot must already exist. Turning it off immediately selects the established
term-addressed frontier. No REINDEX is required because this change adds no
persistent format.

The qualification tools reject an enabled request when the loaded binary does
not expose a registered `pin.enable_owner_frontier` setting. An absent setting
is accepted only for a disabled old-binary control. `PGOPTIONS` and explicit
session checks propagate the requested state to paired psql, psycopg, pgbench,
and CPU-profile connections.

## Correctness qualification

Pure-engine tests cover a 1,024-owner related delta, exact row identities,
bounded posting-tail probes, unrelated-write avoidance, and memory fallback
before emission. The native PostgreSQL qualification adds a 1,024-owner related
fixture with an independent sequential-scan oracle, Boolean query classes,
HOT-eligible updates, indexed-column updates, DELETE, VACUUM, concurrent INSERT,
statement-snapshot retention, immediate crash/restart, and stage 41 selection.
The existing grouped suite continues to cover exact TID reuse, maintenance
publication failures, WAL replay, cancellation, and concurrent maintenance.

Run the local non-PostgreSQL checks with:

```sh
python3 -m compileall -q tools tests
python3 -m unittest discover -s tests -p 'test_*.py'
python3 tools/check_contracts.py
cargo fmt --all --check
cargo test --locked -p pin-core -p pin-kernels
cargo test --locked --release -p pin-core -p pin-kernels
cargo clippy --locked -p pin-core -p pin-kernels --all-targets -- -D warnings
```

The development container completed 123 Python tests and the source contract
check. It has no Rust/PostgreSQL qualification toolchain. Final-head Rust,
PostgreSQL, recovery, and benchmark results must come from CI or a documented
PostgreSQL 18.6 host.

## Paired benchmark procedure

Use a fresh private PostgreSQL 18.6 installation and cluster for every run. The
isolated runner accepts anchor and owner-frontier activation as separate binary
flags. The fourth positional value controls the owner frontier.

```sh
revision=$(git rev-parse HEAD)
export PGRX_PG_CONFIG_PATH=/tmp/pin-g9-owner-off/pg/bin/pg_config
tools/issue14_isolated_run.sh "$revision" .artifacts/owner-off 1 0 \
  --rows 20000 --deltas 0 1 64 512 4096 16384 --related-rows 4096 \
  --samples 6 --queries 100 --seconds 3 --profile-seconds 10 \
  --throughput-seconds 10 --concurrent-seconds 20 --concurrent-related \
  --write-batches 5 --write-rows 10000 --write-update-rows 4096

export PGRX_PG_CONFIG_PATH=/tmp/pin-g9-owner-on/pg/bin/pg_config
tools/issue14_isolated_run.sh "$revision" .artifacts/owner-on 1 1 \
  --rows 20000 --deltas 0 1 64 512 4096 16384 --related-rows 4096 \
  --samples 6 --queries 100 --seconds 3 --profile-seconds 10 \
  --throughput-seconds 10 --concurrent-seconds 20 --concurrent-related \
  --write-batches 5 --write-rows 10000 --write-update-rows 4096
```

Repeat in reversed order and use another fresh cluster for the chosen GIN/base
revision comparison. Do not compare two revisions against one accumulated data
directory. The benchmark checks the loaded full revision, exact row identities,
query plans, and registered activation before timing.

Report each of these classes separately: rare, selective AND, broad AND, broad
OR, NOT/universe, phrase fallback, and absent term. For every delta size report
backend on-CPU p50/p95/p99, client latency p50/p95/p99, buffers, decoded
pages/bytes or available work counters, sampled stacks, allocation/RSS evidence,
index size, write CPU, maintenance CPU, and WAL bytes. Preserve raw commands,
logs, plans, profiles, hardware, PostgreSQL settings, and exact commits.

No result currently exists for owner-frontier on versus off. No speedup,
10x-over-GIN result, write-cost result, or TIN comparison is claimed.

## Remaining work

The experiment still scans one owner payload per post-snapshot live document and
can pay random fragment reads. A measured win would justify a versioned bounded
mutable segment that stores compact page/offset membership as the primary
searchable frontier. A loss would reject this representation or narrow its gate.
Neither outcome can be inferred before profiling.

The ordinary `amgetbitmap` path still enumerates TIDs and loses ranking order.
Exact visible `COUNT(*)` and BM25 top-k require separate executor integrations
with their own MVCC, visibility-map, HOT, quals, RLS, statistics-epoch, and
recovery proofs. They are not implemented or credited here.
