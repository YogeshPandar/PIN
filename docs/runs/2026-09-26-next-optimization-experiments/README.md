# Grouped read experiments against `pin_next.md`, 2026-09-26

This run tested two remaining short-term ideas from `pin_next.md`: fetch only
selected physical bitmap pages, and consume sparse inline catalog postings
without first encoding a temporary bitmap. Neither qualified for merge.
Experimental source diffs, test logs, exact module hashes, settings, SQL,
plans, ordered-row checks, and every backend CPU sample are in `raw/`.

## Method

- PostgreSQL 18.6 on fresh, disposable C.UTF-8 clusters; fsync and full-page
  writes on; 128 MB shared buffers; autovacuum off.
- 100,000 narrow text rows occupy 443 heap pages. Every row contains `wide`;
  one contains `rare`. A grouped PIN index and an expression GIN index exist.
- `enable_seqscan=off`, `enable_indexscan=off`; JSON plans contain bitmap index
  scans. PIN grouped storage/scan are on; the count fast paths are off.
- Each indexed ordered-CTID result equals a forced sequential scan and the
  equivalent other-engine result. Five alternating blocks of 100 warm count
  queries record backend `/proc/PID/schedstat` CPU per query. The values below
  are block medians, in microseconds. They are not p95 latency.
- The first small `pad` fixture compressed to 14 heap pages and did not stress
  physical bitmap records. It is excluded from these results.

| Experiment | Query | `main` PIN | Candidate PIN | GIN in candidate run |
| --- | --- | ---: | ---: | ---: |
| Selected pages, second run | `wide` | 6,651.0 | 6,606.3 | 11,298.0 |
| Selected pages, second run | `rare` | 56.9 | 61.7 | 43.5 |
| Selected pages, second run | `wide AND rare` | 81.7 | 87.3 | 65.2 |
| Compact inline, forward | `wide` | 6,653.9 | 6,692.7 | 11,233.8 |
| Compact inline, forward | `rare` | 59.6 | 65.1 | 44.6 |
| Compact inline, forward | `wide AND rare` | 78.4 | 81.4 | 61.6 |
| Compact inline, reversed order | `wide` | 6,514.5 | 6,554.8 | 11,102.7 |
| Compact inline, reversed order | `rare` | 69.2 | 62.3 | 45.9 |
| Compact inline, reversed order | `wide AND rare` | 86.4 | 88.1 | 64.2 |

The selected-page core test built two physical liveness pages and two physical
posting pages for one group. A selective AND read one of each and returned the
exact expected row. The native selective query nevertheless used more CPU in
both paired runs: 79.7 versus 86.5 microseconds in the first, and 81.7 versus
87.3 in the second. The reader adds directory/extent work and saves at most
two small record pages in this format. Warm PostgreSQL buffer reads were not
expensive enough to compensate. The native runner does not count physical
buffer reads by record, so the per-query read reduction is proven only by the
core PageStore test.

The first inline variant allocated a large fixed array of page masks and
regressed. Compact coordinates reduced that allocation, but the forward and
reverse runs did not establish a reliable selective AND improvement. GIN also
varied across clusters. No production or TIN-level performance claim follows.

Exact tested revisions: `main` `226c877f3d3760de0d0facb87c8b37d18beecfad`;
selected-page variants `faf43f5d5866c07e424d8a3e5d8b39d8cc70de0c` and
`08fa7b41b7e05b7c915d3ffab33ef2fbb30cf9b6`; inline variants
`2ced0814cdd2da85174cfcb9200377bb5eea888e` and
`5ea07a5ef175568b083753a902b5c4b9d8591f71`. The source patches and
module hashes are archived; the experimental code is not enabled on `main`.

## What remains useful from the plan

1. **B1, a packed CTID/page membership primary index.** The current grouped
   bitmap is an accelerator beside canonical owner/posting chains. A direct
   sorted build, versioned manifest, mutable component, and safe retirement
   protocol must replace that double representation. Draft direct-build work
   improves canonical broad reads but does not remove the canonical owner
   traversal or the extra build pass.
2. **A3, physical positional addressing.** Existing selected-term views avoid
   decoding irrelevant positions, but late phrase matches still copy whole
   document fragments. A bounded term/position directory with independent
   record extents is more promising than another group-bitmap read tweak.
3. **A2, native BM25 and top-k.** The pure score oracle is not a SQL ranked
   consumer. Score/order must be checked against all visible qualifying rows
   before safe block pruning. This is necessary for TIN feature parity.
4. **Feature and operations parity.** Bounded fuzzy/wildcard expansion, span
   expressions, analysis profiles, maintenance under concurrent writes, crash
   recovery and standby qualification remain open. These must be measured as
   separate query and write classes, with no weakened visibility rules.

The next performance experiment should measure whole-query CPU after changing
the primary storage and scan path. A 10x improvement over GIN for every SQL
query is not demonstrated here; broad queries also pay heap visibility and
aggregate work that an index codec cannot remove. An actual TIN comparison
requires the same corpus, semantics, engine versions, and server-side metric.

References: [architecture and gates](../../../pin_next.md),
[PostgreSQL index locking](https://www.postgresql.org/docs/18/index-locking.html),
[PlanetScale TIN architecture](https://planetscale.com/blog/introducing-tin).
