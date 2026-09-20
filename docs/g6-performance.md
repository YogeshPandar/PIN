# G6 performance and validation experiments

## Current status

There is no accepted PostgreSQL end-to-end speedup, cost calibration or hardware
parity claim. SIMD selection remains opt-in and does not affect the default
bitmap, count, or visibility paths. The examples below are diagnostic experiments,
not proof that 64-byte containers benefit from an indirect or ISA-specific call.

## Reproduce pure measurements

Use the committed toolchain and lockfile on a controlled machine:

```sh
rustc -Vv
lscpu
sha256sum Cargo.lock
git rev-parse HEAD
cargo run --locked --release -p pin-kernels --example bitmap > bitmap.csv
cargo run --locked --release -p pin-core --example g6_containers > containers.csv
```

The kernel example compares forced scalar and supported AVX2 selection for all
three Boolean operations, empty inputs, small vector tails, heap-page-sized
scratch and larger buffers. Input/output alignment offsets vary. Each output is
compared with a separate per-word reference before and after timing. Fixture
allocation and CPU selection are outside the timed loop. Unsupported AVX2 modes
are omitted, not labelled as measurements of a fallback backend.

The container example compares word-level size selection with an independent
per-offset run counter for empty, singleton, run, dense, alternating and mixed
sets across several valid domains. Encoded sizes, selected tags and cardinality
accompany raw timings. It measures selection, not encoding, disk I/O or SQL.

Five samples alternate the comparison order. Retain each raw row, report
variation, and repeat on the target deployment CPU. Neither harness measures
write p99 or maintenance debt. Do not use noisy shared-runner ratios as release
thresholds. The benchmark examples intentionally use no third-party dependency.

## Observed CI diagnostics

G6 run `35498740070` at
`93c5bbeba0d53c064b6f55a9be4b51849493114c` is useful diagnostic evidence,
not controlled release benchmarking. On that shared runner the current
eight-word `OffsetSet` shape had a lower median per-iteration time through the
forced scalar kernel than through forced AVX2 across samples, operations and
alignment offsets. The same aggregate favored AVX2 at 128 and 1024 words, while
the largest memory-heavy case was near parity.
That size sensitivity is why the production set operations remain scalar and
automatic SIMD is not enabled from this result.

The word-level run counter also agreed with the independent coordinate oracle
while removing most of the per-offset work as domains became denser. The
single-term streaming recheck was faster on the ASCII fixtures in that run,
while the Unicode normalization fixtures were approximately level with the
materialized reference. The PostgreSQL recheck switch therefore remains
default-off until host-level measurements show a useful end-to-end result.

## Reproduce PostgreSQL measurements

Use one fresh benchmark fixture per workload so a faster write run cannot change
the starting state of a later mixed run. The harness requires PostgreSQL 18.6,
an installed Pin extension, a dedicated database, and a superuser because the
experimental fast-path controls are SUSET.

```sh
export PGRX_PG_CONFIG_PATH=/path/to/pg_config
export PGDATABASE=pin_g6

PIN_G6_LABEL=baseline PIN_G6_MODE=core PIN_G6_WORKLOAD=read \
  tools/g6_benchmark.sh
PIN_G6_LABEL=candidate PIN_G6_MODE=count-oracle PIN_G6_WORKLOAD=read \
  tools/g6_benchmark.sh
PIN_G6_LABEL=candidate PIN_G6_MODE=count-stream PIN_G6_WORKLOAD=read \
  tools/g6_benchmark.sh
```

Run write and mixed workloads in separate fresh databases by setting
`PIN_G6_WORKLOAD=write` or `PIN_G6_WORKLOAD=mixed`. The default run uses eight
clients, four pgbench worker threads, a five-second warmup, and thirty seconds of
measurement. Override those values only as part of the recorded benchmark
manifest. Set `PIN_G6_RATE` to add rate control and
`PIN_G6_LATENCY_LIMIT_MS` to define the late/skip threshold.

The harness records the exact commit and Cargo lock hash, PostgreSQL build,
selected server cost and durability settings, CPU metadata, EXPLAIN with buffers
and WAL, relation/index bytes, WAL counters, tuple/dead-row counters, pgbench
output, and per-transaction logs. `tools/g6_latency.py` combines all pgbench
worker logs and reports deterministic nearest-rank p50, p95, p99 and maximum
service latency. For rate-controlled runs it also reports latency from scheduled
start by adding pgbench's documented schedule-lag field.

## PostgreSQL acceptance experiment

Use disposable PostgreSQL 18.6 instances with the same pinned build, durability,
cache state, concurrency, corpus and query semantics. Compare the G6 parent and
candidate commits first. Compare native GIN only on an explicitly matched analysis
subset; Pin's Unicode profile is not universally identical to PostgreSQL simple
text search. Ranking models and VM count eligibility must not be conflated.

Before timing, verify equal result sets and actual EXPLAIN plans. Keep Pin count
experiments off unless separately qualified. Use pgbench custom scripts for rare
and common terms, conjunctions, unions, phrases and negative queries. Include
returned rows, TOASTed text, mixed writes, HOT/non-HOT updates, deletes and VACUUM.
Run long enough to include checkpoint, seal and merge cycles. Measure saturated
and rate-controlled loads separately, retaining errors, timeouts and skipped work.

Record exact source/toolchain/lock hashes, CPU/ISA, kernel/storage, server settings,
private and shared memory, index bytes, WAL bytes, heap fetches, source fanout,
maintenance debt, throughput and p50/p95/p99 latency for reads and writes. Do not
disable fsync, synchronous_commit or autovacuum to manufacture a win. Memory and
write-tail regressions invalidate an unconditional speedup claim.

Only then consider automatic SIMD, different group sizes, read-stream batching,
container thresholds or planner cost changes. Each is a separate ablation and
needs its own correctness and sustained-workload evidence.

## Validation scope

G6 CI compiles, tests debug/release, lints, builds rustdoc and runs the pure examples.
It preserves Cargo/format deltas as review artifacts and rejects uncommitted drift.
A separate pinned test-only nightly job runs Miri on scalar/fallback boundary
cases and AddressSanitizer on native kernels. These jobs do not audit PostgreSQL
MVCC, C buffers, cross-process locks or replay. The existing G0 PostgreSQL workflow
remains responsible for extension compilation and host tests.

Independent unsafe review and end-to-end G6 acceptance remain open regardless of
microbenchmark or sanitizer success.
