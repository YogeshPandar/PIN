# Fenced term-addressed write frontier

Status: implementation and qualification draft for issue #14. Baseline is merged
PR #15, `5afca59199b8ed6ddc3ce6c652d1d4d680562b76`. The supplied ZIP's commit
comment and complete Git tree match remote main, tree
`9e318e140f56d4e1f8a12a82b0e0ca57b1b360a0`. No stale snapshot was used as the
implementation base. Keep #14 open. The 20x backend-CPU reduction and 10x GIN
throughput goals are not achieved claims.

## Evidence and decision

The [September 24 CPU report](runs/2026-09-24-g9-cpu-profile/README.md) and its
raw `perf` stacks describe binary `86d49b9`, before PR #15. In that experiment,
1,000 unrelated inserts caused 1,000 rejected predicate rechecks in both rare
and selective-AND queries. Broad AND instead spent 73.6% of sampled CPU in
PostgreSQL; rare/selective profiles included substantial copying; insert
profiles included page validation. These are different bottlenecks. PR #15's
sparse-term and phrase changes must be remeasured, not credited with these old
numbers or assumed to have eliminated the general Boolean delta.

The existing G9 path combined an exact immutable snapshot with an unconditional
cover of every newer live owner. Every covered owner required heap visibility
and text-predicate work even when none of its terms occurred in the query.
This change removes that end-to-end work for supported Boolean queries, rather
than merely accelerating the bitmap kernel that precedes it.

The chosen design reuses the canonical, term-addressable mutable/sealed posting
chains as the write frontier. It does not introduce a second insert log, a
second publication protocol, or another WAL record per document. Captured term
tails and the grouped snapshot's incarnation fence identify the relevant
suffix. Exact Boolean membership is evaluated before resolving matching
owners to PostgreSQL root TIDs.

[PlanetScale's TIN architecture](https://planetscale.com/blog/introducing-tin)
is a reference for physical tuple identifiers, page-oriented pruning, and
mutable/immutable segmentation. This implementation does not assume TIN's
undocumented synchronization, disk format, or visibility protocol. It retains
PIN's existing grouped page representation and PostgreSQL heap executor.

## Options evaluated

| Option | Benefit and cost | Decision in this PR |
| --- | --- | --- |
| Canonical term-addressable frontier | Avoid unrelated owners and text rechecks without extra insert/WAL work | Implemented |
| Persisted suffix anchors or separate delta tree | Seek directly into long changed-term histories; requires durable anchors, reclamation and migration proof | Next candidate if related-delta measurements justify it |
| Borrow shared PostgreSQL page bytes | Could remove the first page copy, but introduces pin/content-lock lifetimes and error-unwind risks | Not implemented; private-copy contract retained |
| Reuse checked private metadata and bitmap views | Avoid repeated value copies and bitmap decoding without changing host ownership | Implemented |
| Further posting compression | May reduce I/O, but more decoding can hurt sparse queries and needs a versioned format | Existing codecs retained pending profiles |
| More page-level pruning | Useful before offset work; existing PR #15 lower-bound/group pruning already applies | Preserved, not credited as new work |
| Visibility-certified CustomScan count | Can address the bitmap/heap floor, but requires row identity, snapshot, VM ordering, quals/RLS and HOT proof | Not enabled or expanded |
| Position-bearing phrase and ranked retrieval | Avoid repeated analysis and prune ranked work, with new storage/scoring obligations | Existing exact fallback retained; qualification plan below |

## Cost model and implementation

Let `D` be newer owners, `T` the distinct query terms, `P_delta` the relevant
posting references examined, and `M` the candidate owners that satisfy the
Boolean expression. The previous cover adds approximately
`D * (owner lookup + bitmap insertion + heap visit + predicate analysis)`.
The new frontier adds approximately
`T * tail probe + P_delta * membership work + M * owner/heap work`.
These expressions describe work, not measured timing estimates.

For unchanged terms, one captured tail page can establish that no reference
crosses the incarnation fence. Growing an unrelated write delta therefore does
not require walking that owner range for a positively bounded expression. The
regression fixture asserts zero owner-page reads and at most two additional
page reads for two-term queries after 1, 1,000 and 4,096 unrelated inserts.
Those are instrumented core work assertions, not backend CPU measurements.

For a changed term whose suffix starts within its captured tail, the cursor
starts there. If the suffix spans multiple pages, the format has no persisted
seek anchor. The cursor conservatively walks the historical chain. Its cost is
then proportional to that chain, not just `P_delta`. This is an explicit
remaining risk for frequent terms under sustained writes. The benchmark has a
separate related-multipage stage so this regression cannot be hidden by the
unrelated-write case.

An unbounded negation, such as `NOT alpha`, still needs a newer-owner universe.
It enumerates published live owners after the fence, applying exact term
membership. A positive conjunction with a negative filter can instead seek by
its positive terms. Neither path derives a complement over arbitrary heap
slots or joins different incarnations of the same physical TID.

`frontier.rs` owns the bounded stream cursors. `scan.rs` captures term identities
and tails, evaluates the grouped snapshot, releases its payload allocations,
and evaluates the frontier. `storage.rs` returns checked bitmap views that
borrow caller-owned scratch. `page_grouped.rs` supplies a checked private node
view whose binary search reads only keys; a 64-byte value is copied only for a
selected entry. Full page validation on the host read boundary remains intact.

The program is limited to 64 nodes/terms. Its frontier allocation is fallible
and reserved once, with at most one posting image per term and a fixed owner
cache. Grouped payload vectors are dropped before frontier allocation. The
existing budget fallback is retained. This path is bounded-allocation Rust,
not allocation-free `no_std`; the scalar `pin-kernels` contract remains
allocation-free `no_std` and is checked separately in CI.

## Correctness obligations

The snapshot's reserved incarnation is later than every owner included in the
complete snapshot. New references must have a strictly greater incarnation.
Each term captures an inline owner, immutable term identity, and terminal
posting page. An unchanged tail can only exclude new matches when all its
references precede the target. Jumping directly to a tail is legal only when
its first owner is no later than the target, or it is the only posting page.
Otherwise earlier pages must be visited. Posting references remain ordered by
owner page/slot and incarnation; contradictory references fail closed.

All membership is combined using the complete owner identity. Publication and
canonical liveness are checked before emission. Pending or abandoned inserts
cannot manufacture Boolean matches. PostgreSQL's original root TID remains the
heap identity; the frontier never substitutes a dense internal ordinal.

The shared structural reader barrier continues to protect captured sources
against compaction and reclamation. Concurrent appends may create extra
invisible candidates, but cannot remove postings of rows committed before the
query snapshot. Existing writer publication, VACUUM callback authority, grouped
liveness clearing, and WAL/crash protocols are unchanged. Old readers continue
to rely on PostgreSQL snapshot visibility and VACUUM horizons, not an
extension-local xmin/xmax interpretation.

`recheck=false` certifies only the supported text/Boolean index predicate. It
does not certify SQL visibility or authorize index-only counting. PostgreSQL
still checks heap visibility and HOT chains; lossy bitmaps and multiple scan
keys retain their executor recheck behavior. Prefix, phrase, oversized programs
and insufficient budgets keep the existing fallback. Cancellation aborts rather
than returning a partial successful count. Any partial bitmap on an error must
be discarded by the existing host error boundary.

The official contracts and local proof boundaries are recorded in
[API evidence](api-evidence.md). Independent review of concurrency/visibility
remains required; core tests do not replace PostgreSQL qualification.

## Format and operation

There is no new disk format, page kind, WAL record or GUC default. Existing
PIN2, PG09 and G9PG versions are unchanged. Existing grouped snapshots remain
readable and no REINDEX is required for this execution change. Legacy-only
indexes still use their established fallback until an operator explicitly
builds grouped storage. This PR does not silently make an experimental path the
default or rewrite existing indexes.

On a disposable qualification database, build a snapshot and leave subsequent
writes in the canonical frontier:

```sql
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) documents;
SET pin.enable_grouped_storage = off;
SET pin.enable_grouped_scan = on;
SELECT id FROM documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha AND rareplanet');
```

The storage switch above is a benchmark control, not a production maintenance
recommendation. Do not leave a growing delta unmeasured indefinitely. VACUUM
still performs required liveness maintenance with grouped storage disabled.

## Validation and reproducible measurements

The initial implementation commit `62634401827211ea862156d6bef0bd19df01f8d3`
compiled in CI. Issue14 run `35982647506` passed the full debug and release
`pin-core`/`pin-kernels` test suites, all eight new frontier tests, the
allocation-free kernel check and Clippy. Its overall status failed solely at
the formatting gate; the exact formatter patch from Review artifacts run
`35982647569` was applied. G9 grouped-storage run `35982647597` passed. This is
revision-specific evidence, not a claim that later commits automatically pass.
The initial PostgreSQL job `107577954174` in G0 run `35982647514` also passed
native lifecycle, grouped WAL recovery/concurrency and G2 transactional/recovery
qualification. On `4db741005203b2462778ec7fac681477069e8ed9`, Issue14 run
`35984784495`, G6 run `35984784501`, G9 run `35984784571`, and the G0 pure job
passed, including formatting. The extended native SQL suite was still running
when this entry was written. See the PR checks for subsequent-head results.

The development VM has no Rust toolchain and only PostgreSQL 17.10 tools, not
the qualified 18.6 server. Rust was not installed here. All 103 Python tests
passed locally, including the six new benchmark guards. These guards execute
without PostgreSQL or psycopg2 import. No backend CPU,
throughput, WAL, I/O or live performance result for this change is reported
from this VM.

```sh
python3 -m unittest discover -s tests -v
python3 tools/check_contracts.py
cargo fmt --all --check
cargo test --locked -p pin-core -p pin-kernels
cargo test --locked --release -p pin-core -p pin-kernels
cargo clippy --locked -p pin-core -p pin-kernels --all-targets -- -D warnings
```

`tests/sql/issue14_frontier.sql`, included by `g9_grouped.sql`, compares exact
row identities with a sequential PIN predicate and GIN; requires a nonlossy PIN
bitmap plan with zero rejected predicate rechecks; then exercises related
multi-page deltas, aborted inserts, updates, deletion, VACUUM and rebuild. The
existing `tools/g9_qualification.sh` normal and test-hook jobs additionally
cover long readers, concurrent writers, cancellation and restart boundaries.
Run the repository's recovery/standby matrix as well; do not equate a core
publication model with actual WAL replay on a standby.

For paired measurements, install the desired revision into a fresh, disposable,
locally accessible PostgreSQL 18.6 cluster directly under `/tmp/pin-g9-*`, with
PIN preloaded and its extension created. Use a Unix socket, the same machine,
PostgreSQL settings and data parameters for both revisions. Requirements are
psycopg2, the matching `psql`/`pgbench`, Linux `/proc`, and `sudo`/`perf` for the
existing CPU profiler. Hardware PMU counters may be unavailable; they are not
required. With stack profiling enabled, allow `sudo -n perf` and install the
matching debug symbols. Never use a production cluster.

```sh
export PGHOST=/tmp/pin-g9-qualification-socket PGPORT=55432 PGDATABASE=postgres
python3 tools/issue14_frontier_bench.py \
  --disposable --bindir="$(pg_config --bindir)" \
  --revision="$(git rev-parse HEAD)" \
  --output=.artifacts/frontier-head \
  --rows=20000 --deltas 0 1 1000 10000 --related-rows=2048 \
  --samples=3 --queries=20 --seconds=2 --profile-seconds=5 \
  --throughput-seconds=5 --concurrent-seconds=20 \
  --write-batches=5 --write-rows=10000
```

Run the **same new benchmark driver** against a separate clean baseline install
of `5afca59199b8ed6ddc3ce6c652d1d4d680562b76`, passing that exact value to
`--revision` and a separate output directory. The driver checks
`pin.build_revision()` before any DDL and rejects existing fixtures. Do not run
both revisions against one accumulating dataset. It neither installs nor
switches extension binaries. Reverse engine/revision order across repetitions.
Repeat with a larger varied corpus and cold/storage-bound conditions before
making a production claim; the built-in text is deliberately synthetic ASCII.

The driver preserves full identity checks before timing, paired serial counts,
backend schedstat CPU, separately captured `EXPLAIN` plans/buffers, physical
process I/O counters, bounded full-row cursor retrieval, and optional four-client
pgbench throughput with raw per-transaction latency logs and p50/p95/p99. It
retains fresh, each unrelated delta, related-multipage and rebuilt stages
separately. The optional concurrent stage retains an old repeatable-read
snapshot while acknowledged unrelated inserts progress, verifies full identities
again, then verifies a fresh snapshot before maintenance. That is not a
mixed-engine production load generator or a ranked-retrieval benchmark.

Write comparisons use separate PIN and GIN tables, each with the same primary
key, repeated batches and explicit VACUUM after each batch. GIN deferred
maintenance is therefore included rather than hidden behind immediate insert
latency. Per-batch insert and maintenance times, backend CPU, WAL and sizes
remain separate. The read/concurrent fixture has both indexes, so its writes
cannot be attributed to PIN alone. WAL deltas are cluster-wide; backend CPU and
`/proc` I/O exclude work done by other processes outside the measurement window.
PostgreSQL buffer hits do not represent disk reads. All commands, status and
artifact SHA-256 hashes are retained, including on failure.

| Workload class | Current result for this PR | Required next evidence |
| --- | --- | --- |
| Rare terms, fresh | Not measured | PR #15 baseline versus head, sparse path and >64 postings |
| Broad terms and Boolean | Not measured | Heap/bitmap fraction, CPU, buffers, throughput and latency tails |
| Selective Boolean, unrelated writes | Core work-bound regressions passed | Backend CPU, zero rejected heap predicates, delta sweep |
| Related writes | Not measured | Historical-chain traversal, write/read tails and rebuild crossover |
| Phrases | No change to PR #15 phrase execution | Equivalent semantics, varied Unicode and long documents |
| Counts | Ordinary heap-visible bitmap counts only | No custom/VM heap-avoidance claim |
| Full row retrieval | Driver added; not measured locally | Transfer-inclusive cursor costs separately from counts |
| Concurrent writes | Driver and existing qualification paths | Old/fresh snapshots, reader/writer progress and tails |
| Inserts and maintenance | Driver includes deferred maintenance | Lifecycle CPU/WAL/size, repeated batches and background work |

## Remaining architecture work and release gates

A broad result still pays PostgreSQL bitmap and heap costs. Reusing views cannot
remove that floor. A future certified count provider must prove snapshot/VM
ordering after index synchronization, unique logical rows across HOT chains and
incarnations, all SQL quals/RLS, supported isolation and reliable heap fallback.
Merely reading an all-visible bit or counting set posting bits is insufficient.

A viable phrase path would store versioned, owner-bound term positions and use
bounded positional merges after Boolean/page pruning, retaining heap recheck
where positions or analyzer semantics are incomplete. Ranked retrieval further
requires a fixed scoring/statistics epoch and sound upper bounds applied to
visible, eligible rows; fixed oversampling of invisible candidates is not exact
top-k. Neither path is enabled by this PR. They deserve their own format and
recovery qualification rather than an unproved visibility shortcut here.

Remaining gates are final-head native SQL/recovery results, independent review,
paired PR #15/head profiling, broader corpus and cold-I/O coverage, related-delta
regression limits, sustained writer/maintenance tails and RSS/allocation
observations. No benchmark class may be averaged away to meet the stretch
targets. This PR is a tested implementation candidate, not TIN parity or a
qualified production release.

## Follow-up: persisted seek anchors

PR #19 adds a separate default-off, versioned anchor experiment to remove the
historical canonical-chain walk described above. This document's no-new-format
statements describe the original PR #17 frontier, not the anchor extension.
See [snapshot frontier anchors](issue-14-anchors.md) for the persistence protocol,
measured-versus-hypothesized cost, explicit small-history regressions, native
qualification matrix, isolated-build runner and unresolved gates. Enabling a
scan setting without publishing anchors does not exercise the new storage.

## Follow-up: one-pass dense owner frontier

PR #21 adds a separate default-off read experiment for large multi-term deltas.
It evaluates each post-snapshot canonical owner payload once and falls back to
the term-addressed frontier before emission when the operation budget is
insufficient. It introduces no new write representation or WAL format. See
[one-pass dense owner frontier](issue-14-owner-frontier.md) for activation,
correctness boundaries, native qualification, matched commands, and unmeasured
remaining gates.
