# G9 grouped-storage local review

**Code:** `86d49b984df277c14f453698a118a68920b6604b` (merged PR #13)

**Host:** PostgreSQL 18.6 Ubuntu build, Rust 1.98.1, cargo-pgrx 0.19.2, four vCPUs

**Status:** opt-in qualification passed locally; production and TIN parity are not established.

## What changed

G9 adds a supplemental immutable snapshot keyed by 256-heap-page groups and
offset masks. It preserves owner incarnation and shared liveness to prevent an
old posting from matching a reused heap TID. Supported Boolean bitmap scans can
evaluate page groups before loading offsets. The canonical owner/posting store
still accepts writes and remains the fallback for phrase, prefix and unsupported
queries. New writes after a snapshot are returned as recheck-required candidates;
the delta is not yet filtered by query. Building a snapshot is a full rebuild
under the structural and writer barriers. Storage and scan gates default off.

This realizes the page-group representation and shared liveness proposed in
`PERFORMANCE_REVIEW.md`. It is still a supplemental layer, not an incremental
mutable/immutable segment engine or a ranked top-k executor.

## Qualification

The normal and `test-hooks` packages were built with `cargo pgrx package` against
the host PostgreSQL 18.6 installation, then each tested in a disposable cluster
with `fsync`, `full_page_writes` and `synchronous_commit` on. Both
`tools/g9_qualification.sh` modes passed. The hook run observed stages 32, 33,
34, 35, 36 and 38. Both modes observed the memory fallback and sort spill.
The driver compares full row identities with heap scans, including TID reuse,
updates, VACUUM, crash recovery, lossy bitmaps and fallback paths.

The first hook run stopped in its concurrency schedule because the driver waited
for an advisory lock on the maintenance backend. Maintenance actually waits on
Pin's structural heavyweight page lock. The driver now observes that ungranted
page-1 `ExclusiveLock`; the complete hook run passed after this correction.
The qualification script now accepts distribution suffixes after PostgreSQL 18.6.

Also passed: `cargo test --locked --release -p pin-core -p pin-kernels`, all 80
Python tests, `python3 tools/check_contracts.py`, and `cargo fmt --all --check`.
Independent unsafe and storage review remains open. These tests do not establish
steady write throughput, failover behavior, or operational production readiness.

## Controlled scan comparison

The fixture is 20,000 synthetic rows from `benches/g6/compare_setup.sql`, with
the direct-segment option off. A grouped snapshot was built with 64 MB
`maintenance_work_mem`. The same grouped index and binary were then queried
with `pin.enable_grouped_scan` off and on. Each fixed query returned the same
row IDs as the GIN expression-index control. Serial bitmap plans, warm cache,
four clients, two threads, three two-second samples, JIT off and Pin count
shortcuts off were used. These runs measure this fixture only.

One `EXPLAIN (ANALYZE, BUFFERS)` observation per query shows the index-scan
portion below. Times are milliseconds and are sensitive to host contention;
shared-hit page counts show how much index work the grouped path avoided.

| Query | Matching rows | Legacy index hits | Grouped index hits | Legacy index ms | Grouped index ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| `alpha` | 15,000 | 556 | 40 | 4.072 | 1.412 |
| `rareplanet` | 20 | 11 | 16 | 0.148 | 0.210 |
| `alpha AND beta` | 15,000 | 565 | 52 | 7.335 | 1.772 |
| `alpha AND rareplanet` | 15 | 18 | 28 | 0.800 | 0.314 |
| `alpha OR rareplanet` | 15,005 | 558 | 44 | 5.795 | 1.373 |
| `"beta gamma"` | 15,000 | 565 | 565 | 7.594 | 7.822 |

The broad Boolean cases read roughly 11-14 times fewer index pages. The rare
term reads more pages; the phrase query falls back to the legacy executor.
Heap bitmap work remains substantial for broad matches. The grouped snapshot
is not a universal win.

Median throughput, in queries per second, provides a noisy end-to-end check:

| Query | Pin scan off | Pin scan on | GIN in off run | GIN in on run |
| --- | ---: | ---: | ---: | ---: |
| `alpha` | 440 | 623 | 743 | 402 |
| `rareplanet` | 44,717 | 24,897 | 81,400 | 71,291 |
| `alpha AND beta` | 279 | 452 | 650 | 377 |
| `alpha AND rareplanet` | 3,906 | 17,136 | 29,347 | 22,670 |
| `alpha OR rareplanet` | 270 | 486 | 619 | 452 |
| `"beta gamma"` | 5.8 | 4.7 | 3.3 | 2.1 |

The host had significant competing CPU work and the unchanged GIN control
shifted substantially between the two runs. Treat these throughput values as
directional only. They cannot establish that G9 beats GIN. A quiet, paired
rerun with stable controls is required for a speed claim.

After separate clean `REINDEX` operations on the final fixture, the grouped
Pin index occupied 6,397,952 bytes versus 5,480,448 bytes for the legacy-only
Pin index: 917,504 bytes, or 16.7%, extra. This is an index-to-index comparison
on this fixture, not a general storage ratio.

Raw SQL, plans, row-identity JSON, environment JSON and samples are under
`.artifacts/g9-bench-legacy/` and `.artifacts/g9-bench-grouped/` in this
workspace. Those generated artifacts are ignored by Git. The source fixture
and benchmark driver are `benches/g6/compare_setup.sql` and
`tools/fts_compare.py`.

## Next gates

1. Make the mutable delta selective and bounded, then measure query latency as
   writes accumulate after a snapshot.
2. Replace blocking full rebuilds with incremental segmented publication and
   measure write stalls, VACUUM time, WAL volume and crash recovery cost.
3. Reduce catalog/fragment reads for sparse terms and benchmark cold cache and
   larger, varied corpora against GIN under a quiet host.
4. Complete independent unsafe/storage review and native replication tests.
5. Add ranked top-k and visibility-aware execution as separate qualified work.

No TIN benchmark was run. The public architectural resemblance cannot quantify
distance to TIN throughput or latency.
