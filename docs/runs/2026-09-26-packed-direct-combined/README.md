# Packed postings plus direct build checkpoint, 2026-09-26

This checkpoint combines draft PR #31's opt-in packed second posting and draft
PR #32's opt-in direct-TID sealing at index build. Both settings remain off by
default and require superuser access. The combined module reports revision
`f796a3e47548f7d9f8c5d8d1d09c8647523dbdb1`; its compressed binary,
uncompressed SHA-256, build log, raw benchmark samples, plans, SQL, settings,
native lifecycle output, crash-replay output, core tests, and Clippy log are in
`raw/`. The benchmark and native qualification runners are in `tools/`.

## Native result

PostgreSQL 18.6, fresh C.UTF-8 cluster, 16,000 rows, 8,000 rare terms with
two occurrences each, five alternating blocks of 100 warm count queries.
The PIN index uses grouped storage/scan where marked; GIN uses a stored
`tsvector`. Ordered IDs matched forced sequential-scan oracles and GIN, and
JSON plans contained bitmap index scans. CPU is backend `/proc/PID/schedstat`
CPU per query, reported as the median block value in microseconds.

| Layout | Index size without packing | With packing | Broad PIN CPU without/with packing | Selective AND PIN CPU without/with packing | GIN AND CPU |
| --- | ---: | ---: | ---: | ---: | ---: |
| Plain, no direct sealing | 71.55 MB | 6.01 MB | 3,280.9 / 3,133.2 | 494.5 / 416.8 | 115.9–154.8 |
| Plain, direct sealing | 71.73 MB | 6.19 MB | 1,947.4 / 1,612.4 | 366.1 / 314.2 | 115.9–154.8 |
| Grouped, no direct sealing | 72.49 MB | 6.96 MB | 1,402.2 / 1,183.3 | 94.7 / 96.9 | 117.5–117.9 |
| Grouped, direct sealing | 72.41 MB | 6.87 MB | 1,385.4 / 1,159.3 | 98.5 / 99.4 | 117.5–117.9 |

The table compares separate runs in one server; GIN itself varied, so small
CPU differences are not established improvements. The index-size effect is
large and deterministic for this two-occurrence vocabulary. A control with
2,000 terms and eight occurrences per term promoted packed terms to ordinary
postings, leaving the final size unchanged. Reversing the packed/unpacked run
order reproduced the grouped sizes and approximately 97–98 microseconds for
packed grouped AND; it did not reveal a large direct-sealing benefit there.

The combination passed the full `pin-core` suite, 33 native phrase and heap
lifecycle identity checks with both settings enabled, immediate-crash WAL
replay with indexed oracle, and `pin-pg` Clippy with warnings denied. A first
8,000-term attempt failed because disposable clusters filled `/tmp`; its log is
retained as an excluded environment failure. Stopped disposable clusters were
removed before rerunning successfully.

## Scope of the checkpoint

This is a storage/build bridge, not the B1 primary format in `pin_next.md`.
Packed inline terms do not need direct sealing; direct build still performs an
additional pass after writing canonical storage. Grouped page masks still live
beside the canonical owner chain, and grouped selective AND is about 1.2x GIN
here, not 10x. No TIN engine was run. SQL BM25/top-k, fuzzy and wildcard
expansion, span syntax, independently addressable positions, concurrent writer
stress, standby replay, and long-term maintenance remain open gates. The
checkpoint records tested code with default-off controls; it does not declare
production readiness or TIN feature/performance parity.

References: [architecture plan](../../../pin_next.md),
[packed format](../../packed-canonical.md),
[direct-build run](../2026-09-26-direct-build/README.md),
[PostgreSQL index locking](https://www.postgresql.org/docs/18/index-locking.html).
