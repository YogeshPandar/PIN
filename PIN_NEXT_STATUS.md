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
