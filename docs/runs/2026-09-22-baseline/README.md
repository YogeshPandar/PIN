# Local qualification baseline

Tested revision: `0cf93c0776619dcae17fa669f549bed10b50910e`.
Recorded 2026-09-22. PostgreSQL 18.6 upstream commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7`, assertions enabled;
Rust 1.98.1, pgrx 0.19.2, x86_64 Linux. Release extension build.

## Observed checks

63 Python tests, pure Rust nonignored tests, three Unicode conformance tests,
formatting, Clippy, rustdoc, host build and install smoke passed.
Operational qualification passed 234/234 after rebuilding with the required
revision stamp. An initial unstamped development build failed only that stamp check.

The fault suite passed its earlier transaction, crash, count and compaction
stages, then stopped at tools/g7_qualification.py's sealed-prefix retention
assertion. Retained, written and reclaimed pages were all zero. This was one
observed run; a fixture problem versus an implementation defect remains to be
resolved. Subsequent parallel query checks were not executed. No claim that the
entire fault suite passed.

## Small workload measurements

20,000 synthetic documents, about 20 MB heap; four clients, two threads,
prepared statements, five-second warm runs. Private PostgreSQL data on tmpfs.
Durability settings enabled, but this does not measure physical disk durability
cost. Count/VM shortcuts disabled. Bitmap plans forced, GIN comparison serial.

| Query | Pin QPS | Pin average ms | GIN QPS | GIN average ms |
|---|---:|---:|---:|---:|
| alpha, 15,000 matches | 12.12 | 329.677 | 894.92 | 4.470 |
| rareplanet, 20 matches | 7,571.32 | 0.528 | 67,071.81 | 0.060 |

GIN uses an expression index over to_tsvector('simple', body). These ASCII term
queries have matching observed counts; a cross-engine row identity comparison
was not captured. Pin was checked against its sequential predicate. Tokenizer
semantics are not generally interchangeable. Rare-term runs followed an update
of 20 documents and VACUUM. This is a directional baseline, not a statistically
controlled comparison or a PlanetScale TIN benchmark. No scale, write throughput,
soak, or fastest-extension claim follows from these measurements.

Pin's common-term EXPLAIN spent about 3.8 ms in bitmap index production and
208 ms in the heap phase, motivating investigation of predicate rechecks.

Commands: tools/pg_smoke.sh, tools/g8_qualification.sh,
tools/g2_qualification.sh, tools/g6_benchmark.sh. The latter used
PIN_G6_CLIENTS=4 PIN_G6_THREADS=2 PIN_G6_SECONDS=5 PIN_G6_WARMUP_SECONDS=1.
Selected raw output and plan files accompany this report; sha256.json records
their hashes. Disposable servers were stopped; no source changes were made
in this baseline assessment.
