# PR #17 paired CPU review

PR [#17](https://github.com/YogeshPandar/PIN/pull/17) merged to `main` as
`bfcf19dee4e29ef31d6c3e2e9cc1cd4095389b6f`. The comparison baseline is
`5afca59199b8ed6ddc3ce6c652d1d4d680562b76` (PR #15). PR #16 was closed
without merging. This run qualifies a specific read-side regression fix, not a
production or TIN performance claim.

## Method

Both revisions were packaged from independent Cargo target directories and run
in separate disposable PostgreSQL 18.6 clusters on the same four-vCPU GCP VM.
Each cluster kept `fsync`, `full_page_writes`, and `synchronous_commit` enabled,
with autovacuum off. The G9 benchmark used 20,000 synthetic rows, then measured
fresh snapshot, 1,000 unrelated inserts, 64 related inserts across pages, and
rebuilt snapshot states. All four SQL cases compared full row identities to a
heap oracle. The same run included PIN legacy, PIN grouped, and GIN controls.

The values below are medians of two one-second Linux backend on-CPU samples,
in microseconds per query. Each sample repeated the query as many times as the
one-second interval allowed. They include PostgreSQL heap work and query
execution in the backend, but exclude client CPU and other server processes.
The raw archives preserve SQL, query plans, exact row identity checks, buffer
counts, per-sample CPU and runqueue counters, write CPU, WAL and index sizes.

| Stage and case | PR #15 grouped | PR #17 grouped | GIN in PR #17 | PR #15 / PR #17 |
| --- | ---: | ---: | ---: | ---: |
| Fresh, rare | 44.2 | 44.1 | 23.6 | 1.0x |
| Fresh, selective AND | 66.7 | 62.7 | 36.9 | 1.1x |
| Fresh, broad AND | 2,476.7 | 2,408.4 | 3,616.9 | 1.0x |
| Fresh, broad OR | 2,373.5 | 2,327.1 | 3,018.1 | 1.0x |
| 1,000 unrelated writes, rare | 45.2 | 44.9 | 88.4 | 1.0x |
| 1,000 unrelated writes, selective AND | 1,929.3 | 106.5 | 132.6 | 18.1x |
| 1,000 unrelated writes, broad AND | 27,495.0 | 2,517.6 | 3,692.2 | 10.9x |
| 1,000 unrelated writes, broad OR | 27,684.5 | 2,361.7 | 3,105.6 | 11.7x |
| 64 related writes, rare | 1,354.4 | 96.7 | 102.7 | 14.0x |
| 64 related writes, selective AND | 2,043.2 | 2,255.0 | 148.4 | 0.9x |
| 64 related writes, broad AND | 28,831.2 | 6,742.7 | 3,735.6 | 4.3x |
| 64 related writes, broad OR | 30,411.7 | 4,526.7 | 3,172.2 | 6.7x |
| Rebuilt, rare | 67.5 | 64.8 | 35.4 | 1.0x |
| Rebuilt, selective AND | 76.9 | 72.0 | 55.9 | 1.1x |
| Rebuilt, broad AND | 2,499.9 | 2,420.4 | 3,646.5 | 1.0x |
| Rebuilt, broad OR | 2,375.0 | 2,344.0 | 3,088.0 | 1.0x |

In the unrelated-write stage, PR #15's grouped plans emitted 1,000 extra
recheck candidates for selective AND, broad AND, and broad OR. PR #17 emitted
zero in all three while returning the same rows. Its plan returned 20 instead
of 1,020 index candidates for selective AND and 20,000 instead of 21,000 for
the broad cases. The CPU gain is mostly a removal of unnecessary heap work.

Related writes remain expensive. Although PR #17 removed the 1,000 false
rechecks there too, selective AND used 2,255 microseconds of backend CPU versus
148 for GIN. Its instrumented plan spent 2.448 ms in the index scan, returned
84 index candidates, and had zero rechecks. This points to index work on the
actual related frontier as the next measured bottleneck; the current evidence
does not identify a specific instruction or data structure as the cause. The
broad related cases also use more CPU than GIN. After rebuilding the snapshot,
rare and selective AND still use more CPU than GIN, while broad Boolean cases
use less on this fixture.

The small write/maintenance sample did not improve: PR #17 PIN consumed
110.8 ms backend CPU and emitted 3.87 MB cluster WAL for 1,000 inserts plus
VACUUM; the paired GIN sample used 54.1 ms and 0.94 MB. PIN's index was 360 KB
and GIN's 147 KB on this write fixture. These values are narrow workload
measurements, not write throughput or total storage estimates.

## Validity and limits

The first PR #17 package inadvertently reused the baseline Cargo target
directory. Although its build revision stamp said PR #17, its performance and
recheck behavior matched the old core. That run is archived as
`rejected-shared-target.tar.gz` and excluded from the table. The accepted build
used an isolated target directory, compiled `pin-core` and `pin-kernels`, and
removed the false rechecks. A build stamp alone did not establish binary
provenance here.

Both accepted runs completed with exact row-identity checks. PostgreSQL 18.6
plans stayed serial with zero shared reads in the reported CPU plans. Source
contract checks and `cargo fmt --all --check` passed after the merge. The PR's
exact-head CI was green. This local run intentionally disabled call-stack
profiling, multi-client throughput, and concurrent stages, and used two short
samples per case. The host was shared, so these numbers establish a useful
regression signal but not a stable p95/p99 or a 10x-GIN performance claim.

`pr15-raw.tar.gz` and `pr17-isolated-raw.tar.gz` contain each full driver output
directory, including its own file manifest and completion status. The archive
checksums are in `SHA256SUMS`. The nested CPU profiler `environment.json`
obtains `revision` from the calling working directory; for the baseline it
incorrectly records the newer checkout. The top-level driver environment and
each result row's `server_build_revision` contain the verified database binary
revision and should be used for provenance.
