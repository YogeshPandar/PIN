# Predicate streaming and qualification repairs

Recorded 2026-09-22. Same pinned PostgreSQL/Rust/pgrx environment as the baseline.

Ordinary single-term predicates now use the existing exact streaming matcher.
Complex expressions retain the independent materialized oracle. No new unsafe
operations, storage format, dependency, visibility or production cost changes.

The G7 retention fixture previously appended its mutable tail before multiple
VACUUM lifecycle checks. Those checks consumed it. Appending immediately before
the measured reuse VACUUM exercises retention: 10 pages retained, 7 written,
16 reclaimed and 7 reused in the observed run.

After this fix, previously unreached checks exposed planner selection assumptions.
Plain index worker qualification now raises cpu_tuple_cost only for that test;
custom-count worker qualification raises core parallel setup and aggregate
operator costs. Actual plan/worker/result assertions remain mandatory. These
settings isolate worker lifecycle tests; they do not establish production cost
calibration, automatic parallel selection or a performance win.

## Observed verification

- Full tools/g2_qualification.sh completed with exit 0, including transaction,
  crash/recovery, count/recheck, compaction, parallel build/VACUUM, 20 parallel
  query cases, plain index workers, direct-count workers, prepared executions,
  cancellation and worker termination cleanup.
- Nine pure single-term differential tests passed, including exhaustive ASCII
  byte pairs and mixed Unicode documents against the materialized oracle.
- 63 Python tests, host Clippy, formatting and source contracts passed.
- Test hooks were used only in the disposable qualification cluster.

An initial rerun stopped on disk quota before G7; disposable cargo-pgrx build
artifacts were removed before the successful full rerun. Intermediate G7 runs
failed at the plan-selection assertions described above; they were not passes.
A source-inventory check ran while the old predicate was temporarily restored
for rebuilding the benchmark baseline and failed; after restoring the changed
source, all 63 checks passed. Native runtime tests use the compiled library.

## Performance

Normal release baseline: 0cf93c0776619dcae17fa669f549bed10b50910e.
Normal release changed build: 895d271a543ea40015226f53e62a941fd3c95805.
The baseline library was rebuilt with the old matching.rs; other executable
source was unchanged. Benchmark harness changes are separate from that binary.

Same 20,000-document G6 corpus including 20 rareplanet rows, four clients,
two threads, serial bitmap plans, three three-second samples with one-second
warmup per sample and alternating engine order. Both full before/after phases
ran sequentially. Data on tmpfs; PostgreSQL assertions and durability settings
on. Four virtual CPUs, Intel Xeon Platinum 8581C. This shared development VM and
short runs do not establish statistical significance or production throughput.

| Workload | Pin before QPS | Pin after QPS | GIN after QPS |
|---|---:|---:|---:|
| Common term | 11.72 | 12.85 | 908.76 |
| Rare term | 7014.73 | 7615.67 | 63009.37 |
| AND | 11.42 | 11.57 | 769.35 |
| OR | 11.16 | 11.43 | 863.36 |
| Phrase | 14.91 | 15.04 | 3.32 |

Values are medians of per-run throughput. Common-term median per-run p95 changed
from 356.980 to 321.405 ms. Rare-term GIN control varied by 11%, so the observed
8.6% Pin rare-term change is inconclusive. Compound paths were unchanged.

All five cases passed exact cross-engine row-identity comparison in both phases,
with zero benchmark failures. Plans were checked for the intended indexes.
Raw summaries, SQL, plans and per-sample latency statistics are in before/after.
Transaction logs remain in .artifacts/fts-before and .artifacts/fts-after;
their hashes are archived here. Text qualification logs trim trailing whitespace.
No TIN measurement or production-readiness claim is made.

Normal-build G8 completed all 234 assertions after an initial disk-quota failure;
removing disposable debug build artifacts freed space for the successful rerun.
See operational-tap.txt. This is separate from the test-hooks fault qualification.

