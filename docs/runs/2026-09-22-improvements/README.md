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

Repeated normal-build SQL comparison is being collected separately. No TIN
measurement or production-readiness claim is made by these qualification tests.
