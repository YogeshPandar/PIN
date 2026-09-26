# Direct-TID sealing at index build, 2026-09-26

Tested extension binary: `b7c48928fae09aed62c3e9de9f71062b99cbfed3`.
The same PG18.6 binary creates paired new PIN indexes with
`pin.enable_direct_tid_build` off/on, then a stored-vector GIN control.
The packed format is off in both PIN indexes. The server is a disposable
C.UTF-8 cluster with fsync/full-page writes on, autovacuum off, and 128 MB
shared buffers. Each table has 16,000 rows with `echo` in every body and one
of 2,000 `wordNNNNN` terms. The benchmark checks ordered IDs against forced
sequential scans and across all three indexes before timing. JSON plans
contain bitmap index scans. Five alternating blocks of 100 warm queries use
backend `/proc/PID/schedstat` CPU; these are medians of per-block CPU per
query, not p95 latency or TIN measurements.

| Query | PIN canonical | PIN direct build | Stored-vector GIN |
| --- | ---: | ---: | ---: |
| `echo` | 3,115.9 µs | 1,689.6 µs | 1,988.1 µs |
| `word00001` | 82.3 µs | 69.8 µs | 65.6 µs |
| `echo AND word00001` | 714.1 µs | 544.8 µs | 127.0 µs |

Direct-build query CPU is 1.84x better for the broad term. It is 1.18x
faster than stored GIN there, approximately tied on the rare term, and 4.29x
slower on the AND query. Build CPU rose from 951 to 1,099 ms (+15.6%). Index
size rose from 22,396,928 to 22,577,152 bytes (+0.8%); the direct index
contains 2,022 kind-9 pages. The direct conversion is an additional build
pass over canonical postings, not the final packed primary layout.

With `pin.enable_grouped_storage` and `pin.enable_grouped_scan` on for both PIN
indexes, the same fixture measured:

| Query | Grouped canonical | Grouped direct build | Stored-vector GIN |
| --- | ---: | ---: | ---: |
| `echo` | 1,249.4 µs | 1,237.3 µs | 2,017.2 µs |
| `word00001` | 99.9 µs | 72.0 µs | 72.6 µs |
| `echo AND word00001` | 115.4 µs | 105.5 µs | 138.1 µs |

The grouped page masks avoid much of the common-term walk on the AND query.
Direct-build sealing adds little to that grouped path. These results make the
next CPU target specific: use packed CTID/page membership as the primary
Boolean read representation, avoiding owner-reference traversal on fresh
indexes. The present opt-in build pass helps the canonical fallback but does
not achieve TIN-level performance or feature coverage.

The exact binary passed all 33 native phrase and lifecycle identity checks
with direct build enabled, including HOT/indexed updates, deletion, VACUUM,
REINDEX and a repeatable-read snapshot. An isolated immediate-crash replay
verified committed results, exclusion of an in-flight insert, and subsequent
insertion; the replayed index retained seven kind-9 direct pages. The PG
adapter passed Clippy with warnings denied. The branch remains experimental:
concurrent writer stress, standby replay, larger corpora, and full query
feature parity are open.

`raw/plain/` and `raw/grouped/` hold complete per-block samples, settings,
SQL, plans, page census and build WAL deltas. `raw/lifecycle/` and
`raw/recovery/` hold native outputs and server logs. `pin.so.gz` is the exact
tested module; `module.sha256` hashes its uncompressed bytes. Reproduce with
`tools/direct_build_bench.py`, `tools/phrase_lifecycle.py --direct-build`, and
`tools/direct_build_recovery.sh` on disposable PG18.6 clusters.
