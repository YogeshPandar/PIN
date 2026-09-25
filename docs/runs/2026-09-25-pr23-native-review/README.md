# PR #23 native review, 2026-09-25

Code under test: `4c4b19db1e9e528c840786ee747e79706b37ad17` from
[PR #23](https://github.com/YogeshPandar/PIN/pull/23), merged as
`4fff84a2614c2535fc0cde5cb0a7d9fd41113cee`. The only reviewer change
in that commit was `cargo fmt`; the implementation parent is `c0c77cb`.
The release extension's `pin.build_revision()` matched the tested commit.

## Qualification

PostgreSQL 18.6 came from pinned source commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7`, built with assertions,
OpenSSL, and no readline. `cargo pgrx install` succeeded for normal and
`test-hooks` builds. The Grouped COUNT qualification passed in normal mode
and in test-hook mode with frontier anchors and owner frontier. The latter
tests concurrent writer/reader behavior, old snapshots through VACUUM,
writer contention fallback, cancellation, crash recovery, and tuple reuse.
The Python oracle suite (144 methods), source contract checks, format check,
and local `pin-core`/`pin-kernels` Rust tests also passed. All PR checks on
the reviewed head passed, including both complete G0 PostgreSQL jobs, G6,
G9, and Issue 14 contracts; G1 jobs were intentionally skipped. Post-merge
main checks are separate from the PR-head checks.

## Paired release measurement

The archived harness used one backend and alternating order-balanced modes:
previous PIN, grouped PIN, and GIN. It used a durable disposable cluster on
the same four-vCPU GCP VM, 20,000 synthetic rows, six blocks, ten queries per
mode and block, two warmups, and full row-identity and plan checks. All grouped
mode plans executed the grouped path. Numbers below are median backend CPU
microseconds per query across six batches. This is an exploratory warm-cache
small-corpus test, not a 1M-row, concurrent, cold-cache, or TIN comparison.

| Stage | Query | PIN previous | PIN grouped | GIN | Grouped / GIN |
| --- | --- | ---: | ---: | ---: | ---: |
| Fresh after VACUUM | Selective AND | 296 | 216 | 243 | 0.89 |
| Fresh after VACUUM | Broad AND | 1,509 | 259 | 2,486 | 0.10 |
| Fresh after VACUUM | Broad OR | 1,824 | 281 | 2,915 | 0.10 |
| Short write delta | Broad AND | 1,561 | 335 | 2,592 | 0.13 |
| Long write delta | Selective AND | 2,374 | 2,254 | 1,002 | 2.25 |
| Long write delta | Broad AND | 7,557 | 7,117 | 3,655 | 1.95 |
| Long write delta | Broad OR | 7,950 | 7,338 | 4,053 | 1.81 |
| Updated/deleted | Broad AND | 7,962 | 9,639 | 3,590 | 2.68 |
| Updated/deleted | Broad OR | 8,119 | 10,958 | 4,172 | 2.63 |
| Rebuilt by VACUUM | Broad AND | 1,819 | 339 | 3,327 | 0.10 |
| Rebuilt by VACUUM | Broad OR | 2,186 | 300 | 3,525 | 0.09 |

The broad-query improvement is real for this release fixture but conditional
on sealed/clean storage. In the long delta broad AND plan, grouped COUNT read
about 1 MB of index payload and performed 3,121 heap fetches and 3,207 VM
probes; the fresh plan read 43,580 index bytes, performed no heap fetch, and
made 135 VM probes. The unmerged frontier is the next performance target.
All 45 summary cells have `tail_sample_warning` because ten queries per batch
are too few for a credible p95/p99 or production throughput claim. The earlier
debug-build run is excluded from performance conclusions because it changed
the relative costs sharply.

The same run also exposes a large write/storage cost. Creating the PIN index
used 3.18 seconds of backend CPU versus 0.23 seconds for GIN on this 20,000-row
fixture. A separate 100-row insert used 11.2 ms versus 2.3 ms, with 405 KB
versus 55 KB of cluster WAL. Its PIN index occupied 1.78 MB versus 57 KB for
GIN. The 100-row write sample is too small for a stable ratio, but the raw
numbers show that read speed alone cannot justify broad deployment.

The grouped COUNT GUC remains superuser opt-in and default off. The local
qualification does not close independent FFI/MVCC review, allocator/RSS,
concurrent writer latency, cold-cache, or large-corpus gates. There is no
observed TIN parity claim.

`paired-release-20000.tar.gz` contains the raw per-sample CPU records, plans,
cardinality oracles, server settings, source and binary hashes, write costs,
and status. `qualification/` contains the native run statuses and PostgreSQL
logs. The archive's `SHA256.json` covers its individual files.
The archive SHA-256 is
`220026b1ca4215ebf1537633f920c6c8d1b0a1672c5a81fe1855f89113742751`.
