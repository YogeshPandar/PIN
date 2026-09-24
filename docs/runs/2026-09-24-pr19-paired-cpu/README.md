# PR #19 paired CPU and activation review

Code under test: previous main `a2242210b3115a274ebaf4a5a3bec5402fd5d1d3`;
fixed [PR #19](https://github.com/YogeshPandar/PIN/pull/19) head
`ac26b1aff2eb3fb5ecd08abdd7dea2d659a45802`. An initial PR head,
`37093c4198122c3afc0520335fcac23eb7247023`, is retained as a rejected
activation run. This review does not establish 10x-GIN or TIN parity.

## Activation finding

The initial PR #19 head added default-off snapshot-time seek anchors, but
`VacuumStore` did not forward the new `PageStore::frontier_anchors()` method.
PostgreSQL reported the setting as `on` while VACUUM built a snapshot with
`frontier_terms=0`. Its anchored WAL/concurrency CI failed when a query did not
reach the seek hook. The initial paired run showed effectively unchanged CPU
and buffer work. The rejected run is preserved, not averaged into the results.

The one-method fix delegates through `VacuumStore`. With that fix, a local
anchored VACUUM on the benchmark fixture logged `frontier_terms=7`. The
extension's new `tests/sql/issue14_anchors.sql` completed on a disposable
PostgreSQL 18.6 cluster. The PR's normal anchored lifecycle CI passed on the
fixed head. The test-hook recovery/concurrency CI result must be checked on
the exact final head before merging.

## Paired method

Both binaries were packaged from separate Cargo target directories, with
different SHA-256 library hashes. Each ran in a fresh disposable PostgreSQL
18.6 cluster on the same four-vCPU GCP VM. Durability settings were on;
autovacuum was off. The same current benchmark driver ran both revisions.
PIN grouped, PIN legacy, GIN and a heap row-identity oracle ran in each
stage. All five archived runs completed; every inner file-manifest hash
matched. The accepted head had `pin.enable_frontier_anchors=on` on every
measurement connection; the baseline did not have that GUC.

Values are median Linux backend on-CPU microseconds per query from two
one-second samples, with 30 queries per client batch. They include PostgreSQL
index and heap work but exclude client CPU and other backends. Plans had zero
shared reads in these warm CPU measurements. These short serial runs are
regression evidence, not p95/p99, sustained-write throughput or a production
qualification.

### 20,000 rows, then 1,000 unrelated and 64 related writes

| Stage and case | Main PIN | Fixed PR PIN | Paired GIN | PIN gain |
| --- | ---: | ---: | ---: | ---: |
| Fresh, rare | 44.7 | 44.3 | 24.0 | 1.0x |
| Fresh, selective AND | 64.0 | 64.7 | 37.3 | 1.0x |
| Fresh, broad AND | 2,480.3 | 2,447.4 | 3,646.0 | 1.0x |
| Fresh, broad OR | 2,393.8 | 2,340.0 | 3,056.2 | 1.0x |
| Unrelated, rare | 45.2 | 45.8 | 91.1 | 1.0x |
| Unrelated, selective AND | 109.6 | 108.9 | 134.9 | 1.0x |
| Unrelated, broad AND | 2,634.0 | 2,527.6 | 3,755.8 | 1.0x |
| Unrelated, broad OR | 2,434.9 | 2,382.6 | 3,122.7 | 1.0x |
| Related, rare | 98.7 | 99.6 | 103.5 | 1.0x |
| Related, selective AND | 2,284.8 | 176.9 | 149.1 | **12.9x** |
| Related, broad AND | 6,865.7 | 2,604.4 | 3,819.6 | **2.6x** |
| Related, broad OR | 4,684.9 | 2,471.3 | 3,182.3 | **1.9x** |
| Rebuilt, rare | 66.2 | 64.8 | 34.0 | 1.0x |
| Rebuilt, selective AND | 73.7 | 71.3 | 57.5 | 1.0x |
| Rebuilt, broad AND | 2,512.4 | 2,441.4 | 3,645.1 | 1.0x |
| Rebuilt, broad OR | 2,387.9 | 2,363.7 | 3,087.0 | 1.0x |

The related selective-AND plan returned the same 84 rows with zero rejected
rechecks. PIN index shared hits fell from 30 to 25. Related broad-AND hits
fell from 55 to 43. The result supports a real reduction in index work.
Selective AND remains about 1.19x GIN CPU in that stage, while broad AND and
OR are faster than GIN on this fixture.

The separate 1,000-insert-plus-VACUUM write fixture used 117.8 ms PIN backend
CPU on main, 113.2 ms on the fixed PR, and 42.2 ms for GIN in the PR run.
PIN emitted 3,869,776 cluster WAL bytes versus GIN's 944,392. Its index
occupied 368,640 bytes versus 147,456 for GIN. The anchored snapshot rebuild
used 91.6 ms backend CPU versus 88.3 ms on main. These are small fixture
observations, not per-row steady-state write or index-size ratios.

### Longer changed-term suffix

This paired run used 16,384 initial rows, 1,000 unrelated writes and 4,096
related writes, measuring selective AND and broad AND. The related stage
showed:

| Case | Main PIN | Fixed PR PIN | Paired GIN | PIN gain |
| --- | ---: | ---: | ---: | ---: |
| Selective AND | 4,630.8 | 2,880.9 | 1,312.9 | 1.6x |
| Broad AND | 8,291.9 | 4,590.3 | 4,011.7 | 1.8x |

With a longer changed suffix, the anchor still helps but PIN remains about
2.2x GIN CPU for selective AND and 1.1x for broad AND. The remaining cost
needs fresh attribution; these plans alone do not identify its instructions.

## Raw evidence

The five `*.tar.gz` archives retain every driver output: settings, SQL,
row-identity checks, plans, per-sample backend CPU and runqueue counters,
buffer counts, write CPU/WAL/size results, logs, manifests and completion
statuses. `initial-head-rejected.tar.gz` is the zero-anchor run. The archive
hashes are in `SHA256SUMS`; binary hashes and the local anchored SQL result
are in `provenance.json` and `anchored-sql.log`. No perf call-stack capture was
requested in these runs. Hardware-counter, larger-corpus, concurrency,
standby, downgrade and p95/p99 qualification remain separate gates.
