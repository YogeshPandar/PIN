# Packed canonical dictionary qualification, 2026-09-26

This is an experimental opt-in format from branch `codex/packed-canonical`.
The final native binary is commit `c2bba4bc453f6cd4f3bc7cb432d2d69016b5bce3`.
The setting defaults off. The benchmark creates paired fresh tables and indexes
with the same binary, PostgreSQL 18.6, C.UTF-8, fsync/full-page writes on,
autovacuum off, 128 MB shared buffers, and serial warm queries. It then drops
the synthetic tables. Raw SQL, plans, settings, samples, WAL LSN deltas and page
censuses are in `pin-packed-native-c2/`. `pin-c2.so.gz` is the exact native
module used for the final lifecycle, replay and benchmark checks; hashes are in
`SHA256SUMS` and `pin-c2.so.sha256`.

## Final paired results

The corpus has 2,000 distinct `wordNNNNN` terms. Singleton indexes store one
row per word; two-document indexes store two; dense indexes also store `echo`
in each of 4,000 rows. Build CPU is the creating backend's `/proc` scheduler
CPU; WAL is the change in `pg_current_wal_insert_lsn()` for CREATE INDEX.
The query result is median backend CPU per query across five alternating blocks
of 100 `COUNT(*)` queries after warm-up. Both indexed answers matched a forced
sequential oracle, and JSON plans contain a PIN bitmap index scan.

| Corpus | Format | PIN index bytes | Post-VACUUM sealed pages | Build CPU ms | Build WAL bytes | Query CPU µs |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Singleton | Legacy | 4,366,336 | 0 | 102.0 | 5,175,600 | 60.2 |
| Singleton | Packed | 4,366,336 | 0 | 84.0 | 5,207,608 | 58.4 |
| Two-document | Legacy | 20,905,984 | 2,000 | 176.9 | 10,421,464 | 72.1 |
| Two-document | Packed | 4,521,984 | 0 | 164.1 | 10,341,264 | 67.4 |
| Dense plus two-document | Legacy | 21,045,248 | 2,002 | 250.9 | 12,047,248 | 858.6 |
| Dense plus two-document | Packed | 4,661,248 | 2 | 230.8 | 11,967,048 | 842.1 |

The two-document index is 4.62 times smaller because 2,000 dedicated posting
pages disappear. Query CPU falls about 6.6% on that run. Earlier exact-binary
iterations, preserved in `pin-packed-native-run/`, `pin-packed-native-lsn/`,
`pin-packed-native-census/`, `pin-packed-native-direct/` and
`pin-packed-native-direct-repeat/`, show query CPU ranging from a modest win
to a modest regression. Treat read speed as approximately tied at this small
scale. WAL drops less than 1% on the two-document build; size savings do not
imply proportional WAL or latency savings. Build CPU also varies by run.

The first run used asynchronously reported `pg_stat_wal.wal_bytes`, which
returned misleading zeroes. The later LSN-delta runs correct that measurement.
The physical page census was read only after `CHECKPOINT` and VACUUM from this
disposable cluster. Page kinds are counted from the PIN payload tag in the
PostgreSQL page; kind 7 is a sealed posting page. The final raw census includes
all page kinds and post-VACUUM file bytes.

## Correctness evidence and release limits

The full core suite at `c2bba4b` passed 216 tests, with three existing ignored
tests and zero failures. A later focused corruption test passed too. The native
phrase lifecycle passed 33 identity comparisons across build, HOT/indexed
updates, deletion, VACUUM, REINDEX and snapshot behavior. Immediate-crash
replay preserved committed rows, discarded an in-flight insert, and retained
metapage bit 2; a subsequent insert after disabling the creation setting was
searchable. The adapter passed Clippy. Raw logs are included here.

This is a first A1 step, not the full packed arena or B1 primary format. It
does not prove a 10× GIN win or TIN parity; no TIN service was benchmarked.
The 16-byte reserve for every packed dictionary entry needs larger singleton
vocabulary controls. Cross-backend concurrency stress, standby replay, upgrade
policy and the missing TINQL/BM25/highlight features remain open gates.
