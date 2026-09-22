# Exact bitmap SQL checkpoint

Source: bce0749f040577166184dea0b321ed92c06921a3. The inline experiment
adds four measured inline hints to the posting and varint decoders.
Both use the same 20,000-document fixture, serial bitmap scans, 4 clients,
2 threads, 3 alternating samples of 3 seconds. Counts/VM shortcuts and JIT
are disabled. Each case passes symmetric row identity comparison with GIN.
Warm tmpfs, PostgreSQL 18.6 with assertions, Rust 1.98.1, four virtual CPUs.
These are local directional measurements, not TIN or production comparisons.

| Case | Exact Pin QPS | Inline Pin QPS | Inline GIN QPS | Inline Pin p95 ms |
| --- | ---: | ---: | ---: | ---: |
| common | 426.55 | 457.83 | 804.39 | 9.839 |
| rare | 31000.82 | 32696.34 | 53757.56 | 0.230 |
| and | 325.34 | 361.80 | 690.49 | 12.036 |
| or | 381.68 | 404.67 | 764.50 | 11.026 |
| phrase | 13.25 | 13.20 | 2.93 | 312.441 |

Prior common-term streaming baseline: 12.85 QPS. Removing unnecessary
predicate rechecks accounts for the large improvement. Inline hints contribute
about 6-11% on common/AND/OR in this run, with roughly stable GIN controls.
Rare-term results are less conclusive because the GIN control moved. Phrase
queries retain rechecks. GIN's phrase path is particularly expensive on this
fixture; this is not a general phrase-search performance claim.

Validation at bce0749: pure Rust suite, 63 Python tests, Clippy with warnings
denied, full G2, and 234/234 normal-package G8 checks passed. The four inline
hints also pass the full pure Rust suite. The inline experiment package has
revision unrecorded; it was not represented as a stamped release.
One earlier G2 run failed to observe a paused parallel VACUUM worker while
other tests were active; the subsequent idle complete run passed. Worker
assignment remains a harness reliability issue.

Software cpu-clock profile: 4,997 samples, no lost samples. Posting decoders
and varints dominate, followed by page copying (libc rep movsb), heap work,
and canonical owner resolution. Hardware performance counters were unavailable.
The profile is statistical and includes PostgreSQL executor work.

Remaining gaps: Pin remains slower than GIN on positive Boolean counts; no
ranked SQL implementation, production certification, or TIN feature parity.
Canonical owner references in postings still require a heap-TID lookup.
