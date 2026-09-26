# Selected positional execution: native qualification

Tested binary: `66c6c1e86cb46d59b5544e15b0e63ab8f2b78a06`, PostgreSQL 18.6,
Rust 1.98.1, pgrx 0.19.2, GCP VM. The code enables fragmented phrase proofs,
selective term-position decoding, shortest-list phrase anchoring, and one reusable
bounded fragment buffer. It remains opt-in through `pin.enable_phrase_positions`.

## The useful result and the remaining failure

The implementation removes substantial reanalysis/decoding work, but it has **not
reached the 10x-over-GIN target against stored-tsvector GIN**. The strong stored
GIN control is faster on all tested cases. Avoiding expression reanalysis explains
much of the apparent advantage against the earlier GIN comparison.

Warm, serial backend CPU per query, medians of six alternating batches of six
queries, 256 documents with 10,000 repeated tokens and two ordering tokens:

| Phrase | PIN heap recheck | PIN selected positions | GIN expression index |
| --- | ---: | ---: | ---: |
| alpha beta | 195.081 ms | 1.702 ms | 1366.125 ms |
| echo echo | 191.713 ms | 16.744 ms | 1370.636 ms |
| alpha echo (no matches) | 197.570 ms | 24.086 ms | 1372.198 ms |

All variants use ordinary bitmap heap scans and identical count result shapes;
the custom count shortcuts are disabled. The PIN scalar sequential oracle,
legacy PIN, selected PIN, and GIN produced identical ordered `id,ctid` streams.
Plans report zero shared-buffer reads. GIN uses an expression index over
`to_tsvector('simple', body)` and repeats that expression during heap rechecks.
This deliberately preserves the previous comparison; it is not the strongest GIN
configuration for repeated searching.

A separate control adds a stored generated `tsvector` and a GIN index on it.
Both compared engines query the same expanded heap. Six alternating batches of
100 queries provide the following CPU medians:

| Phrase | PIN selected positions | GIN stored vector | PIN / GIN CPU |
| --- | ---: | ---: | ---: |
| alpha beta | 1.696 ms | 0.110 ms | 15.4x |
| echo echo | 16.831 ms | 0.138 ms | 122.3x |
| alpha echo (no matches) | 24.103 ms | 0.210 ms | 114.6x |

Stored vectors add persistent storage and write work; this run measures query CPU,
not total lifecycle cost. Full result identities still agree for these queries.
These synthetic documents and three phrases are not a general tokenizer or
position-domain equivalence claim.

The short-document control uses 2,048 rows with 32 repeated tokens, four
alternating batches of 100 queries, and the same stored-vector comparison:

| Phrase | PIN selected positions | GIN stored vector | PIN / GIN CPU |
| --- | ---: | ---: | ---: |
| alpha beta | 1.337 ms | 0.629 ms | 2.1x |
| echo echo | 1.481 ms | 0.641 ms | 2.3x |
| alpha echo (no matches) | 1.817 ms | 0.703 ms | 2.6x |

The separate short-document expression-index run is also retained in raw data.
Do not combine ratios across the expanded and unexpanded heaps.

## CPU evidence and next implementation target

The final binary's software-clock profile captured 1,689 samples, zero lost, over
5,488 warm adjacent-phrase queries. Flat/self samples put **45.29% in memmove**,
5.62% in the scan/phrase-resolution function, and 4.80% in page loading. The full
report and `perf.data` are retained. Some callchains are incompletely unwound;
flat samples do not identify the exact source of every copy. The profile is a
separate workload, not part of the timing batches.

Hardware `cycles,instructions` are unsupported on this VM, including with root
perf access. No IPC, cycle count, hardware cache-miss rate or branch-miss claim
is made. Raw schedstat snapshots, per-batch CPU deltas, client timings and all
SQL/plans are preserved. Some short batches carry the driver's short-duration
warning; these are batch CPU medians, not p99 estimates.

Selected decoding now avoids unrelated deltas, but legacy fragment access still
loads/copies complete document payloads. Queried common terms still decode their
complete occurrence lists. The next A3/B1 step needs direct term/position-block
addressing and bounded block iteration that can stop at a phrase witness. A
reusable in-place page reader may reduce large Rust page moves, but even removing
all sampled copying would yield less than 2x on this profile: it cannot close the
15x-or-larger stored-GIN gap alone. Packed canonical records and native score/count
consumers remain separate necessary workstreams. Native SQL BM25 is still missing.

## Correctness, scope and exploratory evidence

- Core: 197 tests passed; three existing Unicode data tests ignored.
- Query suite: all 16 passed, including independent text oracles, fragmented
  order/repetition, insufficient budget, damaged chains and consumed corruption.
- Clippy: all core targets passed with warnings denied.
- Native lifecycle: all 33 phrase/stage identity comparisons passed, including
  HOT/indexed updates, own writes, rollback, delete, VACUUM, REINDEX, and an old
  repeatable-read snapshot retained across committed changes.
- No storage-format, WAL, new unsafe operation or MVCC shortcut was introduced.
  The query reader's corruption policy is narrower: unused deltas and global
  cross-term uniqueness belong to the retained full validator.

The earlier full-fragment decoder experiment measured 18.734 ms for the adjacent
phrase, versus 1.702 ms for the final selective reader (roughly 11x). That earlier
binary was a dirty exploratory build without immutable source provenance, and
short Rust test runs overlapped some batches. Its library hash, complete samples
and limitations are retained; use the immutable final binary's paired controls
for conclusions. The initial harness syntax failure and the C-locale Unicode
mismatch are retained too. Final qualification uses `C.UTF-8`.

This is warm, serial, small synthetic qualification. There is no TIN service run,
realistic corpus claim, new crash/replay matrix, sustained-write throughput test
or production latency claim. Current coverage and remaining architecture work
are in [PIN_NEXT_STATUS.md](../../../PIN_NEXT_STATUS.md).

## Reproduction and evidence

Build the recorded source using the exact package command in `raw/build.json`.
Use a new unprivileged PostgreSQL 18.6 cluster initialized with UTF-8 and locale
`C.UTF-8`. Point `shared_preload_libraries` to the packaged library and
`extension_control_path` to its share directory; set the packaged control's
module_pathname to that same absolute library. Keep fsync/full_page_writes on.
The captured postgresql.conf records the exact isolated paths used here.
Create extension pin, set PGHOST/PGPORT/PGDATABASE to the disposable cluster, then:

```sh
python3 tools/phrase_lifecycle.py --output /tmp/lifecycle-new
python3 tools/fragment_phrase_bench.py --disposable --output /tmp/long-new
python3 tools/fragment_phrase_bench.py --disposable --stored-control --queries 100 --output /tmp/stored-new
python3 tools/fragment_phrase_bench.py --disposable --rows 2048 --tokens 32 --blocks 4 --queries 12 --output /tmp/inline-new
python3 tools/fragment_phrase_bench.py --disposable --stored-control --rows 2048 --tokens 32 --blocks 4 --queries 100 --output /tmp/inline-stored-new
```

Output paths must not already exist. Run measurements sequentially. The final
benchmark driver is preserved under raw/ as well as tools/. The long expression
run used the same driver before optional token/stored-control arguments were
added; its literal corpus and complete executed SQL are preserved.
`raw/pin.so.gz` contains the exact final module and symbols, with decompressed
SHA-256 in `raw/build.json`. The evidence ledger covers every raw file.
