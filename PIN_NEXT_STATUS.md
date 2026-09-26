# Implementation status against pin_next.md

The specification in [pin_next.md](pin_next.md) is the destination, not a claim
that PIN already implements it. The current iteration starts from merged PR #27
and implements part of A3. CI is not on the local iteration critical path.

## Implemented in this iteration

- Selective PD02 positional views validate the term directory and decode only
  requested term streams. Whole-document validation remains a separate API.
- Fragmented documents can prove root-level phrases using index positions.
- One bounded fragment buffer is reused for a scan. Its retained capacity and
  the plan fit one query budget; oversized payloads preserve heap recheck.
- Phrase execution chooses the shortest occurrence list as its anchor.
- An explicit work assertion selects two positions from a 10,002-token document.
- A standalone native benchmark preserves SQL, full identities, plans, settings,
  raw backend CPU observations and warm per-request client timings.

The query reader intentionally validates consumed data; unused positional,
directory and fragment tails may remain unchecked after an exact proof. The full validator checks those properties.
A successful query is not an integrity check. There is no new unsafe boundary,
WAL format, custom visibility implementation or change to transaction semantics.
The phrase feature remains opt-in through `pin.enable_phrase_positions`.

## What this does not solve

The PD02 bridge now skips physical tails when a first-fragment phrase witness
proves the result. Late and negative matches can still require a full payload
copy and positional traversal. It does not yet offer direct position-block seeks. Nested
phrase/Boolean expressions still use their existing conservative paths. SQL
BM25, competitive top-k, fuzzy/wildcard/span parity, and the new primary storage
format remain outstanding.

| Workstream | Current status | Next concrete proof |
| --- | --- | --- |
| A0 work accounting | Selected-position counts and reproducible phrase CPU harness | Per-query source/byte/candidate/visibility counters across consumers |
| A1 packed canonical storage | Not implemented | Eliminate dedicated pages for tiny lists without moving dictionary identities or losing captured-reader tails |
| A2 SQL BM25 | Pure oracle exists; no native ranked scan | Exhaustive eligible-row score/order parity under one coherent statistics context |
| A3 selected positions | Inline/fragmented bridge implemented | Directly address selected term extents; nested positional execution |
| B1 primary layout | Design only | Typed codecs, format migration, direct build and complete maintenance recovery |
| C1 count lifetime | Existing global protection retained | Retained-generation retirement and native reuse/interleaving qualification |

Native BM25 needs separate correctness and measurement from `ts_rank_cd`.
TIN's default scoring can omit dense terms; full scoring retains them. Omitting
terms must be a named scoring policy, not a hidden optimization. The next ranked
executor must establish its competitive threshold only from visible rows that
pass security and residual predicates.

## Why the remaining redesign matters

`writer::link_term` still promotes the second occurrence to a dedicated posting
page. Shared records should address that allocation pathology, but packing alone
cannot remove PostgreSQL's heap/slot/projection work or supply ranked execution.
GIN itself stores heap item pointers: CTID-native identity alone does not explain
a speed advantage over GIN. Selected positions, avoided text reanalysis, different
result consumers and safe score pruning are the mechanisms to qualify.

The result record is [selected position qualification](docs/runs/2026-09-26-selected-positions/README.md).
It uses a synthetic long-document fixture and separate short-document control;
neither establishes production p99 or a direct comparison with the TIN service.

References: [PostgreSQL GIN storage](https://www.postgresql.org/docs/18/gin.html#GIN-IMPLEMENTATION),
[PostgreSQL scan contracts](https://www.postgresql.org/docs/18/index-scanning.html),
[TIN architecture](https://planetscale.com/blog/introducing-tin),
[TIN scoring](https://planetscale.com/docs/postgres/search/scoring).


## Prefix-witness checkpoint

The next measured iteration adds lazy occurrence cursors and physical tail
avoidance. Compared with the previous reader, long-document adjacent/repeated/
negative phrase CPU improves about 1.9x/19.4x/2.4x. Stored-vector GIN remains
faster. Short-document PIN CPU is about 1.2–1.7x GIN in this fixture.
[Full results and raw profiles](docs/runs/2026-09-26-prefix-witness/README.md)
record the exact builds and limits. [TIN reuse](docs/tin-reuse.md) records what
is public and why Lead's scanning engine is unsuitable as a performance base.

## Seekable position block experiment

PB01 now provides independent position-block restart points and bounded seek
work in pure Rust. [Format and native integration gates](docs/position-blocks.md)
and [raw kernel measurements](docs/runs/2026-09-26-position-blocks/README.md)
are tracked. Late seeks skip most decoding, while early witnesses regress;
retaining the prefix path is necessary. Core validation: 206 passed, 3 existing
ignored, Clippy clean. This codec is not wired into PD02 or SQL. A3 direct
physical reads and B1 native format/writer/maintenance migration remain open;
no new native performance gain or TIN parity is claimed for this experiment.

## Native dense-position iteration after PR #28

PR #28 is merged on main. The next native branch combines page-return inlining
with bounded 32-occurrence canonical delta-run skips. The long negative phrase
measured 1.55–2.23 ms backend CPU versus baseline 10.15–17.73 ms across two runs.
Matching phrases improve more modestly; VM variation and GIN controls are kept
in the [full results](docs/runs/2026-09-26-native-dense-seek/README.md). Long PIN
still costs roughly 3.7–7.6x stored-vector GIN CPU. The short repeated phrase is
approximately tied, not a demonstrated general win. Native lifecycle: 33 passed.

The new cpu-clock profile still puts 30.92% of samples in memmove. A3 direct
physical reads, budgeted reusable page buffers, B1 native storage migration,
and SQL ranked/TIN feature parity remain unfinished. No production-ready or TIN
performance claim follows from this synthetic phrase optimization.

## A3 private page reuse

The next reader iteration implements `PageStore::read_into`, exclusive image
reload and one budgeted fragment image retained across candidates. This follows
A3's adapter-first sequence in `pin_next.md`; it does not complete B1 physical
position addressing. Native long-phrase CPU falls another 29–43% in the paired
runs; memmove falls from 30.92% to 15.39% of sampled CPU. Long PIN still uses
approximately 3.2–5.5x stored-vector GIN CPU. Core: 210 passed, 3 existing ignored;
native lifecycle: 33 comparisons passed; Clippy clean.
[Full measurements and limits](docs/runs/2026-09-26-page-reuse/README.md).

The plan remains useful and incomplete: A1 packed canonical postings, A2 SQL
BM25, B1 physical position directories and subsequent ranked/span execution are
still substantial work. Recent bridge optimizations do not establish final
production readiness or TIN parity.

## B1 document extent prototype (not qualified)

The experimental `pin.enable_direct_documents` creation option adds a versioned
physical map and selected positional reads. Fixed 3KB heads regressed size by
49% on the 16K fixture; adaptive heads restore the legacy page count. Retaining
tail scratch reduces reader overhead, but final mapped late-match CPU remains
0.931 ms versus legacy 0.785 ms and stored GIN 0.133 ms in the paired run.
Native lifecycle passes 44 comparisons. This is physical addressing groundwork,
not the final packed layout, performance parity or permission to enable by default.
[All iterations, failures and raw evidence](docs/runs/2026-09-26-document-extents/README.md).

Next: profile the remaining virtual-reader overhead, measure a native longer
fixture where physical skipping actually avoids pages, and qualify crash replay.
Keep both the positive work-bound test and the negative two-page native result.
