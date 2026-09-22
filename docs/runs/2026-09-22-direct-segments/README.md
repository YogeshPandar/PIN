# Direct sealed posting checkpoint

The final tested source is `dbae7afd376e083e7e2c580d617ac682047bb76c`.
The `reference` and `sealed` measurements used binary `60fdc8a8203ae32c23192a2655a771af402c4313`;
`fast` used the stamped final binary and a newly reindexed tag-9
index. `reference` kept ordinary owner-based sealed pages; `sealed` converted
them to the experimental direct format before the single-term streaming change.
These are sequential local development checkpoints, not an isolated A/B test
of every change.

The fixture has 20,000 synthetic documents on warm tmpfs. PostgreSQL 18.6
with assertions, Rust 1.98.1, four virtual CPUs, four pgbench clients, two
threads and three alternating three-second samples were used. Plans force
serial bitmap scans, disable JIT and Pin count/VM shortcuts, and use exact
bitmaps. Each case checks symmetric row identity with GIN before timing.
The archived JSON contains all samples; the table gives median QPS and final
Pin p95 service latency. No TIN instance was tested.

| Case | Reference Pin / GIN QPS | Sealed Pin / GIN QPS | Final Pin / GIN QPS | Final Pin p95 ms |
| --- | ---: | ---: | ---: | ---: |
| common | 458.13 / 790.26 | 705.14 / 791.57 | 878.58 / 785.80 | 5.054 |
| rare | 32061.19 / 50262.77 | 34644.47 / 52891.29 | 34665.51 / 53276.59 | 0.218 |
| AND | 350.12 / 688.66 | 477.60 / 689.97 | 465.58 / 685.07 | 9.412 |
| OR | 396.27 / 763.14 | 572.16 / 761.09 | 578.66 / 763.81 | 7.689 |
| phrase | 13.06 / 2.88 | 13.18 / 2.85 | 13.33 / 2.90 | 307.875 |

The final direct index occupied 5,652,480 bytes after reindex, VACUUM and
CHECKPOINT, versus 5,578,752 bytes before conversion, about 1.3% more
allocated file space. Physical tag counts after CHECKPOINT were 85 tag-9
pages out of 690 total pages. The common-term result exceeds GIN in this one
fixture. Rare, AND and OR are still behind GIN. These measurements do not
establish production performance or PlanetScale TIN parity.

Validation on the final source: release pure Rust suite, warning-free release
Clippy, 63 Python tests, contract checks, full G2 including direct SQL mutation
and parallel cases, and normal package G8 (234/234). A disposable PostgreSQL
cluster survived immediate postmaster shutdown and WAL replay after direct
compaction, DELETE, VACUUM and TID replacement; indexed identities matched a
sequential scan after restart. `tests/` and `recovery/` preserve the outputs.

A software `cpu-clock` profile of 6,000 repeated AND counts collected 4,997
samples with no lost samples. About 40% self samples appear under `Cursor::seek`,
8% under page validation, and 6% in PostgreSQL heap HOT search. Decoder calls
are partly inlined into `Cursor::seek`, so the profile does not isolate a single
instruction or prove which algorithm would win. Hardware counters were
unavailable.

An attempted two-term merge by heap TID was rejected by the pure query oracle:
distinct live owner incarnations can share a TID in the model, and merging only
coordinates can invent an AND hit. The attempt was reverted. Any page-group
bitmap design must retain an incarnation proof or generation-scoped liveness.

Reproduce against a prepared `pin_g6_bench` fixture with
`tools/fts_compare.py --bindir PATH --output PATH --samples 3 --seconds 3 --exact-bitmap on`
(the saved `environment.json` and SQL files specify the run) and
`tools/direct_recovery.sh`. The local test logs
record the exact commands and stamped revision. The benchmark fixture is small,
hot, read-only during measurement, and has no sustained write, cold-storage,
replica, or high-cardinality coverage.
