# Snapshot frontier seek anchors

Status: opt-in implementation and qualification follow-up for PR #19. Native
qualification of this follow-up and backend performance qualification are pending.
This document does not authorize a 10x-GIN, TIN-parity, or production-readiness claim.

## Source identity

The uploaded `PIN-main (7).zip` identifies commit
`a2242210b3115a274ebaf4a5a3bec5402fd5d1d3`. Recreating Git blob modes and its index
produced tree `67c0bb66e4dab3f52db241a7c3133d2094b1c6e3`, exactly matching remote
`main` when checked. The existing PR head is
`2ab08406e7bb8670aa2faf4375ef4f93a668bb53`, tree
`ba9f54294cbef27d3f0af2cc82e1b549fb93ce48`. Its source was independently restored
from the CI review artifact and verified against that tree before continuing.
The CI artifact names a merge-preview commit, not the PR commit; equality of
source trees, not that name, established the implementation base.

`Cargo.lock` remains byte-identical to the ZIP, SHA-256
`409da258dfc596e362031de56f970e6acc081aca4615d68ebd2ef3291ae39534`.
No Rust installation was attempted in this development environment.

## Observed bottleneck and limits of attribution

The accepted [PR #17 raw run](runs/2026-09-24-pr17-paired-cpu/README.md) measured
PostgreSQL 18.6 on a warm, serial, synthetic 20,000-row workload. All three raw
archives, including the rejected shared-target run, were hash-checked, including
552 manifest entries per archive. The rejected run remains excluded.

After 1,000 unrelated and 64 related writes, selective AND returned the same 84
rows with PIN and GIN. Both instrumented plans had zero rejected rechecks, zero
shared reads, and 13 exact heap blocks. PIN's index node reported 30 shared hits
and 2.448 ms; GIN's reported 12 hits and 0.161 ms. Total instrumented execution
was 2.483 and 0.198 ms respectively. Separately measured backend on-CPU cost was
2,255.0 and 148.4 microseconds/query. These observations locate the regression
in index work rather than the previously corrected false-recheck problem.
EXPLAIN node time is not an on-CPU attribution percentage.

The relevant path is `amgetbitmap` through the existing guarded PostgreSQL
storage adapter, `grouped::scan_query`, dictionary/tail capture, immutable
page/offset Boolean evaluation, `frontier::scan`, deferred owner resolution,
and the existing batched TIDBitmap sink. PostgreSQL then performs heap/HOT
visibility checks and executor work. The frontier may visit the historical
posting chain when its current tail begins after the snapshot fence. In
particular, unrelated writes create an incarnation gap; several related suffix
pages can then make a current-tail jump unsafe.

The core counter test demonstrates that such a chain walk occurs and that an
anchor removes historical posting reads and copied payload bytes. It does not
establish what fraction of the historical backend CPU was validation, copies,
decoding, branches, buffer calls, Boolean work, owner resolution, bitmap insertion,
or PostgreSQL overhead. The paired historical run disabled sampled stacks and
hardware counters. That complete attribution remains a VM gate, not an invented
profile.

## Algorithm and cost model

At grouped snapshot publication, capture each canonical term's chain head,
then-current tail, and last owner incarnation. Store one ordered anchor record
per dictionary term in a separate catalog owned by the same grouped allocation
journal as the bitmap snapshot. The catalog uses explicit byte encoding, not
Rust object layout. A term contributes a 32-byte sort record and an 80-byte
catalog entry before page/internal-node overhead. Maintenance reserves another
bounded catalog builder; native tuplesort retains responsibility for spill.

The scan keeps the existing cheap tail tests. An unchanged term needs no anchor
lookup. If the current tail itself safely covers the target, it is used directly.
Otherwise a valid anchor seeks to the captured boundary page, verifies the
captured final incarnation, and visits only that boundary and subsequent pages,
stopping at the scan's captured canonical tail. Inline-only snapshot terms use
their captured first incarnation and the subsequently created canonical chain.

For historical pages H, suffix pages D, catalog depth L, and one boundary page B,
the affected cursor changes from O(H + D) page visits to O(L + B + D). This is a
work bound, not a backend speedup claim. Query term count and existing scratch
budgets still bound cursor memory. There is no new per-candidate allocation and
no new unsafe operation. The frontier still evaluates full owner incarnations,
then resolves surviving owners to physical TIDs. NOT still uses the published
owner universe when a positive bound cannot cover it.

The change does not optimize heap visibility, bitmap insertion, position checks,
or all rare-query overhead. A short history can gain nothing and pay extra
catalog reads. Snapshot rebuild adds catalog/sort/WAL cost. Ordinary insert
publication does not update a separate anchor for each write.

TIN's [public architecture description](https://planetscale.com/blog/introducing-tin)
documents physical tuple identifiers, 256-page grouping with offset bitmaps,
term counts, and mutable/immutable segments. Those are architectural references.
This implementation uses a persisted boundary on PIN's canonical chains; it does
not reproduce TIN's complete organization or independently establish its safety
protocol. TIN was not run on a matched workload.

## Persistence, locking, recovery and fallback

`pin.enable_frontier_anchors` is a default-off superuser setting, separate from
`pin.enable_grouped_storage` and `pin.enable_grouped_scan`. An enabled rebuild
publishes PG09 metadata version 2 with the anchor root and validity bit. The
same binary reads legacy version 1. Unknown versions/flags, wrong identities,
malformed anchor records, missing roots and broken ordering fail closed.

A grouped reader retains the existing shared structural barrier. Canonical
maintenance takes the exclusive structural barrier before the writer interlock.
Before a canonical chain can be replaced or recycled, compaction commits anchor
invalidation through the existing WAL page-store boundary. It does so regardless
of current experiment settings. Recovery rejects an impossible live-anchor and
rewrite-journal overlap. Anchor and bitmap allocations are recovered together;
a catalog root need not be the last page in the allocation journal.

Stage 39 observes durable invalidation. The follow-up adds stage 40 after a
validated anchor seek, where no PostgreSQL page borrow or content lock escapes.
The existing privileged test-only injection mechanism can prove path selection,
pause a reader, or raise an error. Normal builds expose no injection SQL and the
host event implementation is a no-op. Tests must show that an append completes
while a reader pauses there, while canonical maintenance waits for the structural
barrier. An error after any earlier emissions aborts the scan; it is not partial
success and must not leave a consumable bitmap.

The default-off, invalidated and version-one paths retain the canonical walk.
Unsupported syntax and insufficient budgets retain the existing pre-emission
fallback. A malformed enabled anchor is an error, not permission to silently
skip committed matches. Existing incarnation liveness, VACUUM retirement before
heap-slot reuse, MVCC and HOT responsibilities are unchanged. Index membership
never proves a visible SQL `COUNT(*)`.

Turning the setting off is **not a binary downgrade**. Version-two metadata and
its pages remain persistent. Before an old binary can read the index, stop using
the experiment and REINDEX under the current binary with grouped storage and
anchors disabled, or drop/recreate the index. Qualification of that deployment
procedure, standby replay, upgrades and interrupted downgrade remains required.
Do not enable this format on a production installation yet.

## Results and regressions

These are historical PR #17 medians, not measurements of the ZIP or new head.
The recorded server revision is `bfcf19dee4e29ef31d6c3e2e9cc1cd4095389b6f`.
Two one-second samples per case are insufficient to qualify tail latency.
A fresh exact-ZIP build and a fresh new-head build must be compared using the
same current driver. Pending is not zero and is not a claimed improvement.

| State | Query class | Historical PIN CPU, us/query | New PIN | Paired historical GIN CPU, us/query |
| --- | --- | ---: | --- | ---: |
| Fresh | Rare | 44.1 | Pending | 23.6 |
| Fresh | Selective AND | 62.7 | Pending | 36.9 |
| Fresh | Broad AND | 2,408.4 | Pending | 3,616.9 |
| Fresh | Broad OR | 2,327.1 | Pending | 3,018.1 |
| 1,000 unrelated | Rare | 44.9 | Pending | 88.4 |
| 1,000 unrelated | Selective AND | 106.5 | Pending | 132.6 |
| 1,000 unrelated | Broad AND | 2,517.6 | Pending | 3,692.2 |
| 1,000 unrelated | Broad OR | 2,361.7 | Pending | 3,105.6 |
| 64 related | Rare | 96.7 | Pending | 102.7 |
| 64 related | Selective AND | 2,255.0 | Pending | 148.4 |
| 64 related | Broad AND | 6,742.7 | Pending | 3,735.6 |
| 64 related | Broad OR | 4,526.7 | Pending | 3,172.2 |
| Rebuilt | Rare | 64.8 | Pending | 35.4 |
| Rebuilt | Selective AND | 72.0 | Pending | 55.9 |
| Rebuilt | Broad AND | 2,420.4 | Pending | 3,646.5 |
| Rebuilt | Broad OR | 2,344.0 | Pending | 3,088.0 |

Broad OR here means `alpha OR rareplanet`. Common-only, bounded NOT, phrase,
prefix and absent-query cases have no paired backend result in this historical
run; the driver accepts them, but their new and baseline results remain pending.
All historical cells fail the numeric 10x-GIN threshold. No new-head 10x target
has been tested or qualified. Rare/selective queries after rebuild and broad
related-write queries remain documented regressions against GIN, not omitted
cases.

| Historical insert 1,000 + VACUUM | PIN | GIN | New PIN |
| --- | ---: | ---: | --- |
| Backend CPU | 110.764 ms | 54.143 ms | Pending |
| Cluster WAL | 3,869,368 bytes | 944,392 bytes | Pending |
| All index bytes on write fixture | 360,448 | 147,456 | Pending |

WAL is cluster-wide. Update CPU, update WAL, continuous related-write throughput,
new-format maintenance cost and meaningful p95/p99 remain unmeasured. The concurrent benchmark supports acknowledged unrelated or related writes
on a dual-index table and holds a repeatable-read snapshot. It is not a per-engine write-CPU comparison
or a saturation test of continuously changing matching terms.

The initial PR head's [core CI job](https://github.com/YogeshPandar/PIN/actions/runs/36008246679/job/107661929870)
reported these deterministic **core work counts**, with 16,384 historical owners,
1,000 unrelated writes and 64 related writes:

| Query | Posting reads, old/new | Posting bytes, old/new | Total reads, old/new |
| --- | --- | --- | --- |
| Rare | 3 / 3 | 2,177 / 2,177 | 10 / 11 |
| Selective AND | 12 / 6 | 53,600 / 4,596 | 22 / 18 |
| Broad AND | 18 / 6 | 102,846 / 4,838 | 28 / 18 |
| Broad OR | 12 / 6 | 53,600 / 4,596 | 22 / 18 |

Owner reads stayed at one. With 2,048 historical owners, posting work was unchanged
and total reads rose from 11 to 12 for rare and 17 to 19 for the Boolean cases.
This explicitly records the extra lookup cost. These are not PostgreSQL buffers,
backend CPU, GIN comparisons or evidence of TIN parity.

## Qualification and reproducible commands

The development follow-up passes 117 Python tests, source contracts, Python
compilation and shell syntax checks. Two additional Rust tests cover failure
before/after the invalidation commit and actual seek-event error propagation.
They join nine existing anchor tests. They have not been compiled here. The old
CI head passed its core tests, Clippy and API docs but failed formatting; its
exact Rust 1.98.1 formatter patch was applied as a separate commit. Its native
PostgreSQL tests ran with anchors disabled and do not qualify this follow-up.

`tests/sql/issue14_anchors.sql` adds the 16,384-row history, version-one upgrade,
unrelated and related writes, long changed suffix, own/aborted writes, eligible
HOT and indexed-column updates, deletes, disabled-gate VACUUM and rebuild.
Full row-identity comparisons use forced heap scans and matched GIN predicates.
The existing lifecycle suite, including observed TID reuse, now also runs with
anchors enabled. The test-hook matrix covers invalidation crash/WAL replay,
concurrent read/append/maintenance, repeatable read and cancellation. Source/mock
tests of that matrix are not substitutes for its PostgreSQL execution.

On the native VM with the pinned toolchain already provisioned:

```sh
cargo fmt --all --check
cargo test --locked -p pin-core
cargo test --locked --release -p pin-core --test issue14_anchors -- --nocapture
cargo check --locked -p pin-kernels --no-default-features
cargo clippy --locked -p pin-core --all-targets -- -D warnings
cargo clippy --locked -p pin-pg --features test-hooks --all-targets -- -D warnings

# use a disposable postgres 18.6 installation, never a production prefix.
export PGRX_PG_CONFIG_PATH=/tmp/pin-g9-qualification/pg/bin/pg_config
cargo pgrx init --pg18 "$PGRX_PG_CONFIG_PATH"
(cd crates/pin-pg && cargo pgrx install --pg-config "$PGRX_PG_CONFIG_PATH")
PIN_G9_ANCHORS=1 bash tools/g9_qualification.sh
(cd crates/pin-pg && cargo pgrx install --features test-hooks --pg-config "$PGRX_PG_CONFIG_PATH")
PIN_G9_ANCHORS=1 PIN_G9_TEST_HOOKS=1 bash tools/g9_qualification.sh
```

The G0 workflow contains both anchored runs as well as the existing default-off
runs. Preserve its entire `.artifacts` upload, including failed runs.

For performance, provision **a different private PostgreSQL prefix for each
candidate**, built from upstream commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7` with identical compiler/configuration.
Each prefix must contain no installed `pin.so`. For example, with an existing
PostgreSQL source checkout in `PIN_PG_SOURCE` and native build prerequisites:

```sh
pg_commit=724edf9bde9d356724ad384a2e196edc3c9f80f7
test "$(git -C "$PIN_PG_SOURCE" rev-parse "$pg_commit^{commit}")" = "$pg_commit"
for name in baseline candidate; do
  prefix="/tmp/pin-g9-$name"
  mkdir "$prefix"
  mkdir "$prefix/source"
  git -C "$PIN_PG_SOURCE" archive "$pg_commit" | tar -x -C "$prefix/source"
  (cd "$prefix/source" && ./configure --prefix="$prefix/pg" --enable-cassert --with-openssl &&
    make -j2 && make install) > "$prefix/postgres-build.log" 2>&1
done
```

Retain both PostgreSQL build logs. Initialize cargo-pgrx for the
chosen prefix before its run. Start from a clean checkout of the follow-up and
make both source commits available locally. Set `head` to the full code commit
being qualified, not the original PR commit:

```sh
baseline=a2242210b3115a274ebaf4a5a3bec5402fd5d1d3
head=$(git rev-parse HEAD)
export PGRX_PG_CONFIG_PATH=/tmp/pin-g9-baseline/pg/bin/pg_config
cargo pgrx init --pg18 "$PGRX_PG_CONFIG_PATH"
bash tools/issue14_isolated_run.sh "$baseline" "$HOME/pin-baseline-run" 0 \
  --rows 20000 --deltas 0 1000 --related-rows 64 \
  --queries 100 --samples 6 --seconds 5 --write-batches 3 --write-rows 1000
export PGRX_PG_CONFIG_PATH=/tmp/pin-g9-candidate/pg/bin/pg_config
cargo pgrx init --pg18 "$PGRX_PG_CONFIG_PATH"
bash tools/issue14_isolated_run.sh "$head" "$HOME/pin-candidate-run" 1 \
  --rows 20000 --deltas 0 1000 --related-rows 64 \
  --queries 100 --samples 6 --seconds 5 --write-batches 3 --write-rows 1000
```

The runner requires fresh worktrees, Cargo target and intermediate build
directories, empty compiler-wrapper overrides, locked prebuild, unchanged
lockfile after cargo-pgrx installation, source archives and copied installed
library hashes. It uses a new disposable cluster for each candidate. The same
current driver tests both binaries; nested Git provenance runs from each source
worktree. A registered `pg_settings` row must match requested activation on every
measurement backend. An unknown custom-GUC placeholder is not activation proof.
The initial benchmark did not propagate this flag into its child profilers;
this follow-up corrects that failure mode.

Repeat in new directories/prefixes with `--related-rows 4096`, larger corpora,
all nine query classes, `--throughput-seconds 30` and `--concurrent-seconds 30`.
Add `--concurrent-related` to change matching terms during that stage. Add
`--write-update-rows 500` to measure indexed-column UPDATE CPU/WAL separately
on the per-engine write fixtures; its row count must not exceed `--write-rows`.
Lifecycle totals then include insert, update and maintenance, and are not the
same workload as the historical insert-only totals. Defaults preserve the
original insert-only and unrelated-write cases.
`--profile-seconds 10` additionally requires permitted `sudo -n perf` recording.
Hardware PMU unavailability is retained, not converted into zero cycles.
A requested stack-profile failure is a failed run, not a successful empty
profile. All raw SQL, plans, samples, profiler failures, source/library hashes,
server logs, exit status and evidence checksums are retained. Build and cluster
directories remain available for inspection and require later manual cleanup.
The isolated runner itself still needs native end-to-end validation.

Before enabling by default: obtain native matrix results on the exact head,
independent storage/locking review, standby and downgrade tests, complete profile
attribution, update and sustained related-write costs, and repeated paired
GIN results with stable latency distributions. A core work reduction closes none
of those gates by itself.
