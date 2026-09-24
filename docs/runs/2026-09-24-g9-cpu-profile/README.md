# G9 backend CPU and I/O attribution

**Date:** 24 September 2026

**Extension binary:** `86d49b984df277c14f453698a118a68920b6604b`

**Status:** measured in a disposable PostgreSQL 18.6 cluster; no production or TIN speed claim.

This run measures CPU work that the earlier throughput comparison could not
attribute. It includes unprofiled on-CPU time for individual PostgreSQL backends,
separate sampled call stacks, buffer counts, process I/O counters, a write-delta
experiment, insert batches and VACUUM. [Every raw sample and recording](raw/) is
committed alongside this report. Compressed `perf.data` files are lossless and
have original and archive SHA-256 hashes in each directory's `MANIFEST.json`.

## Host and measurement contract

The host has four virtual CPUs presented as Intel Xeon Platinum 8581C under KVM.
It is **not a bare-metal measurement**. Linux `perf` 7.0.14, `psycopg2` 2.9.12
and matching `libc6-dbg` symbols were used. The extension shared library contains
debug symbols; the packaged PostgreSQL executable is stripped but exposes many
function symbols. `fsync`, `full_page_writes` and `synchronous_commit` were on.
The disposable cluster used 128 MB `shared_buffers`, autovacuum off and a Unix
socket. The initial read fixture held 20,000 synthetic rows and both a Pin and a
GIN expression index. The source is `benches/g6/compare_setup.sql`.
Its repeated vocabulary made the GIN index only 155,648 bytes versus 6,397,952
bytes for the grouped Pin index. This small, warm fixture cannot model large,
varied corpora or cold storage.

The primary CPU metric is the first field of Linux
[`/proc/<pid>/schedstat`](https://www.kernel.org/doc/html/latest/scheduler/sched-stats.html):
nanoseconds a single PostgreSQL backend was scheduled on a CPU. The second field
records runqueue waiting. Each read sample used a prepared `SELECT count(*)`,
one client and a fresh backend, warmed for 0.5 seconds, then ran for two seconds.
Three samples alternated engine order. CPU time per query is the backend's
on-CPU delta divided by completed queries. It includes PostgreSQL query dispatch,
planning/execution, Pin or GIN, bitmap/heap work and visibility checks; it
excludes the Python client and other PostgreSQL processes. It is not hardware
cycles, instructions or per-function exact CPU accounting.

For attribution, separate five-second `perf record -e cpu-clock -F 199 -g
--call-graph dwarf,8192 -p <backend PID>` captures sampled user and kernel call
stacks. Those captures may perturb throughput, so the main CPU numbers come from
the unprofiled samples. Flat percentages are fractions of *sampled backend CPU*,
not fractions of the unprofiled query's elapsed time. `libc6-dbg` resolved the
otherwise anonymous libc copying routines. All reported profiles except the
explicitly noted short VACUUM capture had zero lost samples. `perf` raw data,
stack reports, flat reports and shared-object summaries are under `raw/`.

`perf stat` rejected hardware events: `cycles` is unsupported by this VM, even
with `sudo`. Therefore **no cycle count, IPC, hardware cache-miss count or branch
miss count was measured**. See [the raw PMU probe](raw/g9-cpu-full/hardware-pmu-probe.log).
The source profiler records a fresh probe on subsequent runs. A bare-metal host
with an exposed PMU is needed for those counters.

For every fixed query, the tool checked symmetric `EXCEPT ALL` row identities
between Pin and GIN before timing and verified a bitmap plan on the expected
index. Both engines used `simple`-style queries on this fixture. This is equality
for these SQL cases, not proof of general analyzer or phrase equivalence.
`EXPLAIN (ANALYZE, BUFFERS)` was captured once per engine/query separately from
the repeated CPU samples. Its time is wall time; its shared-buffer hits and reads
are an I/O-work observation.

## Fresh snapshot, no new write delta

Median backend CPU microseconds per completed query:

| Query | Legacy Pin | G9 Pin | GIN | G9 / GIN CPU | G9 / GIN index buffer hits |
| --- | ---: | ---: | ---: | ---: | ---: |
| `alpha` | 4,916 | 2,535 | 2,797 | 0.91 | 40 / 6 |
| `rareplanet` | 45.6 | 64.2 | 23.6 | 2.72 | 16 / 2 |
| `alpha AND beta` | 8,769 | 2,759 | 3,392 | 0.81 | 52 / 13 |
| `alpha AND rareplanet` | 667.5 | 113.4 | 57.7 | 1.96 | 28 / 7 |
| `alpha OR rareplanet` | 7,150 | 2,627 | 2,929 | 0.90 | 44 / 7 |
| `"beta gamma"` | 217,160 | 216,378 | 732,892 | 0.30 | 620 / 12 |

All the one-shot plans recorded **zero shared-buffer read blocks**: this was a
warm-cache run. Hits are not disk reads. Broad Boolean G9 queries used less
backend CPU than GIN on this fixture despite touching more index buffers.
Rare and selective queries remained 2.7x and 2.0x as CPU expensive as GIN.
The phrase case fell back to Pin's legacy executor and is very slow in absolute
terms; GIN was slower for this specific repeated-text fixture. Only a few
phrase executions fit in each two-second sample, so that ratio needs separate
validation on varied text and semantics.

The profiles explain different parts of these costs:

| Workload | Sampled attribution |
| --- | --- |
| Rare G9 | 35.0% libc, 29.6% PostgreSQL, 23.5% Pin, 11.0% kernel. `__memmove_avx512_unaligned_erms` alone was 24.7%. Call stacks include Pin page loads and grouped record reads. |
| Selective AND G9 | 38.8% libc, 31.5% Pin, 20.4% PostgreSQL. `memmove` was 31.2%; Pin group-node metadata and page loading also appeared. |
| Broad AND G9 | 73.6% PostgreSQL, 17.8% Pin. `heap_hot_search_buffer` was 11.8% and Pin's grouped evaluator 4.6%. The heap/executor now consumes most sampled CPU in this case. |
| Phrase Pin | 93.0% Pin. `unicode_segmentation::tables::word::word_category` was 60.6% and `Analyzed::analyze` 21.8%, consistent with repeated analysis of candidate text. |

The C storage adapter copies page contents from a shared buffer into a private
page under a lock; the Rust page loader subsequently validates that private
page. The profile's `memmove` call stacks include these loads. We cannot assign
every libc copy sample to that one call site: other copying may share the same
symbol. See `pin_storage_read` in `crates/pin-pg/cshim/pin_storage.c` and
`Page::read_with` in `crates/pin-core/src/mutable/page.rs`.

Supplemental four-second profiles for common and OR queries were captured later,
after 2,000 unrelated inserts, their grouped rebuild and separate write tests.
They are in `raw/g9-cpu-broad-profile/`. Their elapsed/CPU values changed with
the larger fixture and cache state, so they are used only for attribution. The
sampled G9 backend CPU was about 78-80% PostgreSQL, principally heap/bitmap
execution. Do not splice those absolute times into the initial table.

## Write-delta CPU cliff

The first 1,000 inserted rows contained only `unrelated filler`, so they matched
neither `rareplanet` nor `alpha AND rareplanet`. The grouped snapshot was left
unchanged. The single insert statement used 72.6 ms of backend CPU and advanced
cluster WAL by 4,415,736 bytes; this statement updated the heap, primary key,
Pin and GIN indexes, so those values cannot be attributed to Pin alone.

| Query | Fresh G9 CPU | G9 after 1,000 writes | G9 after rebuild | GIN after writes | G9 index candidates / rejected rechecks after writes |
| --- | ---: | ---: | ---: | ---: | ---: |
| `rareplanet` | 64.2 µs | 1,199 µs | 63.3 µs | 85.2 µs | 1,020 / 1,000 |
| `alpha AND rareplanet` | 113.4 µs | 1,841 µs | 116.3 µs | 150.3 µs | 1,015 / 1,000 |

The grouped rare and selective queries consumed **18.7x** and **16.2x** their
fresh-snapshot CPU. PostgreSQL rejected exactly the 1,000 unrelated candidates.
In the stale rare G9 profile, 77.8% of sampled CPU was in Pin; Unicode word
classification alone was 30.8%. The selective profile showed the same pattern.
GIN also slowed after inserts, consistent with work on its pending list, but
remained much cheaper on these two queries. Legacy Pin remained near its baseline.
This isolates the cost of Pin's conservative newer-owner cover on this fixture.

`VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0)` with grouped storage enabled
used 961.6 ms backend CPU and advanced WAL by 1.81 MB; it restored the G9
selective CPU values above. That operation includes ANALYZE, so it is not a
grouped-build-only cost. A separate `VACUUM` without ANALYZE after another
1,000 new rows used 99.2 ms CPU and 507 KB of WAL; its `perf` capture contained
no samples and is retained as a negative raw trace. The base fixture remained
disposable throughout.

## Insert and maintenance CPU

Separate logged tables had identical schemas, one primary key and one text
index each. One used Pin; the other used a `simple`-configuration GIN expression
index with PostgreSQL's [default `fastupdate` behavior](https://www.postgresql.org/docs/18/gin.html).
Each sample inserted
50,000 rows of `repeat('alpha beta gamma ', 32) || (id % 100)::text` in one SQL
statement. The two engine orders were reversed between samples. Pin grouped
storage was off during these inserts. Results therefore measure the immediate
insert path, while GIN may defer work to pending-list maintenance.

| Engine | Backend CPU per inserted row, samples | WAL per row, samples | Text index after insert |
| --- | ---: | ---: | ---: |
| Pin | 116.94 / 117.08 µs | 3,998.5 / 3,998.5 bytes | 15,179,776 bytes |
| GIN | 41.02 / 40.63 µs | 893.4 / 893.4 bytes | 4,038,656 bytes |

Pin's immediate insert consumed about 2.9x the backend CPU in this specific
batch. In its two write profiles, `Page::validate` occupied 27.6-28.2% of
sampled backend CPU, libc `memmove` 16.1-20.7%, and Unicode word classification
5.9-10.2%. Those are statistical shares, not proof that removing one operation
would save the same fraction of total time. The shorter 15,000-row pilot and
its raw recordings are also retained under `raw/g9-cpu-write/`.

On one 50,000-row Pin table, a later grouped snapshot `VACUUM` used 1.096 s of
backend CPU, advanced WAL by 2.62 MB and left a 27,697,152-byte Pin index.
Its 217-sample profile attributed 30.9% to libc, 25.8% to Pin, 21.7% to the
kernel and 21.2% to PostgreSQL. `memmove` was 25.8%; owner reading, page
validation, kernel copying and page checksums also appeared. On one GIN table,
`VACUUM` used 88.4 ms CPU, advanced WAL by 30.50 MB and left a
4,399,104-byte index. Its profile had only 17 samples, too few for a reliable
function breakdown. These are one operation each with different maintenance
work and are **not** a steady-state total-cost comparison.

A separate cluster-process sample captured the backend, WAL writer and
checkpointer counters around one more 50,000-row insert per engine. The VM's
load average rose sharply during that capture, increasing Pin CPU to 148.8
µs/row and GIN to 49.9 µs/row. It is archived under
`raw/g9-cpu-write-system/` and **excluded from the primary comparison**. In
that contended sample, the WAL writer accumulated 28.9 ms CPU alongside a
7,440 ms Pin backend window, and 11.4 ms alongside a 2,496 ms GIN backend
window. Background work can occur outside those short windows; those values
are not a full system-lifecycle allocation.

The `/proc/<pid>/io` `read_bytes`/`write_bytes` fields were mostly zero during
the warm query and write statements. PostgreSQL writes through shared buffers
and WAL/background processes, so zero backend physical-write bytes does **not**
mean zero database I/O. Raw per-process syscall, fault, context-switch,
on-CPU and runqueue deltas are in each `results.json`.

## Raw evidence and reproduction

| Directory | Contents |
| --- | --- |
| [`raw/g9-cpu-full/`](raw/g9-cpu-full/) | 20,000-row read samples, plans, environment, 12 query profiles, two VACUUM observations and PMU probe |
| [`raw/g9-cpu-delta1000/`](raw/g9-cpu-delta1000/) | 1,000-new-row samples and six query profiles |
| [`raw/g9-cpu-rebuilt/`](raw/g9-cpu-rebuilt/) | post-rebuild recovery samples |
| [`raw/g9-cpu-broad-profile/`](raw/g9-cpu-broad-profile/) | later common/OR attribution profiles |
| [`raw/g9-cpu-write/`](raw/g9-cpu-write/) | 15,000-row write pilot, four profiles |
| [`raw/g9-cpu-write-long/`](raw/g9-cpu-write-long/) | 50,000-row write samples, Pin/GIN VACUUM counters and profiles |
| [`raw/g9-cpu-write-system/`](raw/g9-cpu-write-system/) | contended process-tree sample, retained but excluded |
| [`raw/g9-cpu-smoke/`](raw/g9-cpu-smoke/) | initial short CPU measurement probe |
| [`raw/g9-cpu-profile-smoke/`](raw/g9-cpu-profile-smoke/) | initial rare-term sampling and symbolization probe |

Each directory has `MANIFEST.json`. The relevant directories also have
`results.json`, readable `*.flat.txt` and `*.dso.txt` symbol reports, full
`*.txt` stack reports, `runner.log`, and losslessly compressed `*.data.gz`.
To inspect one raw profile,
restore it and open it with a compatible `perf` installation and matching
debug-symbol packages:

```bash
gzip -dc raw/g9-cpu-full/rare-grouped-perf.data.gz > /tmp/rare-grouped-perf.data
perf report --stdio -i /tmp/rare-grouped-perf.data
```

The profiler sources are `tools/g9_cpu_profile.py`,
`tools/g9_cpu_write_profile.py` and `tools/g9_cpu_archive.py`. They require
`psycopg2`, `psutil` for process-tree write capture, `perf` and passwordless
`sudo` for this host's profiling policy. The write tool requires an explicit
`--disposable` argument and a `/tmp/pin-g9-*` cluster; it creates new synthetic
tables. The original uncompressed `perf.data` SHA-256 is retained in every
manifest so the compressed upload can be verified after download. A scan of
the synthetic-cluster artifacts found no GitHub token, authorization-header,
password-environment or API-key marker among the checked patterns.

## Claim boundary

This evidence identifies expensive work on the tested paths: grouped sparse
page copying and metadata access, PostgreSQL heap/bitmap work for broad matches,
text analysis in phrase and false-positive rechecks, and Pin page validation
and copying on inserts. It does not give CPU cycles or cache misses, does not
cover all PostgreSQL background work over a sustained period, and does not
measure TIN. The VM is shared and the data is synthetic. The raw recordings
allow these observations to be revisited; they do not establish bare-metal,
large-corpus or production performance.
