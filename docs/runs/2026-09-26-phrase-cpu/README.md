# Inline phrase and syscall qualification, 26 September 2026

This run used an isolated PostgreSQL 18.6 cluster on a GCP VM, 20,000 initial
synthetic rows plus 4,096 appended rows, then updates, deletes, and VACUUM.
All SQL results were checked before timed measurements. The phrase ablation
compared the full ordered `id,ctid` stream for the prior PIN path, indexed
positions, and GIN, then ran six balanced blocks of twelve warm COUNT queries
per mode. Backend CPU comes from `/proc/<pid>/schedstat`; client latency is
reported separately. Plans show ordinary bitmap heap scans, no custom COUNT
shortcut, zero shared-buffer reads, and exact heap pages. The complete input,
source identity, plans, row identity checks, samples, and syscall logs are in
[`raw/`](raw/).

| Rebuilt fixture, median backend CPU | Prior PIN | PIN positions | GIN |
| --- | ---: | ---: | ---: |
| `"bravo charlie"` | 79.76 ms | 27.53 ms | 218.95 ms |
| `"delta echo"` | 79.22 ms | 28.84 ms | 218.07 ms |
| `"charlie bravo"` | 80.67 ms | 26.88 ms | 220.04 ms |

The position path was built from commit
`e176feeb58449d935a79a3607b15dc7549993a4e`. It removes heap text
reanalysis for inline indexed phrase matches, while PostgreSQL still checks
heap visibility. It is about 7.6 to 8.2 times faster than GIN for these three
phrase COUNT cases. It does not improve ranked top-k or broad row retrieval.
The merged PR #26 baseline measured, after rebuild, 6.47 ms PIN versus 8.08 ms
GIN for broad rows and 272.03 ms versus 273.17 ms for top-k using the same
`ts_rank_cd` expression. The baseline's `phrase_count` median was 79.57 ms PIN
versus 218.07 ms GIN. Those controls and every lifecycle stage are in
[`raw/merged-baseline/`](raw/merged-baseline/).

The syscall ablation was a negative result. On 40 warm prior-PIN phrase queries,
the backend made 50,320 `lseek` calls and no file reads; the bounded-read
experiment made 200 `lseek` calls for 40 queries. The position phrase median
was 27.53 ms before the cache and 27.59 ms with cache plus bounded C reader;
prior-PIN phrase was about 79 ms in both runs. The incremental C boundary and
concurrency proof were therefore reverted. The intermediate binaries and
samples remain in `raw/phrase-cache/` and `raw/phrase-bounded/` for audit.
`strace` itself perturbs latency, so its output is used only for syscall counts.

The lifecycle script performed 24 full identity comparisons across initial
build, HOT update, indexed text update, self-visible uncommitted insertion,
rollback, delete, VACUUM, and REINDEX. It included repeated words, Unicode,
and a fragmented document. The pure `g4_query` suite passed 13 tests.
This is a warm, serial, small synthetic fixture. It does not establish
production p95/p99, concurrent-write latency, crash recovery, standby replay,
or direct comparability with PlanetScale's TIN benchmark.
