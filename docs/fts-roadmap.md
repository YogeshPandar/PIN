# Fast, robust native PostgreSQL search

Research refreshed 2026-09-22. `pin_plan.md` is the design charter; its phase
names do not establish that every phase's acceptance criteria have passed.
The objective is competitive open-source search with independently reproducible
correctness and performance. There is no workload-independent "world's fastest"
metric. Publish measured comparisons with corpus, semantics, concurrency,
memory, durability, maintenance state and hardware disclosed.

## Current priorities

1. Remove measured ordinary predicate overhead. The first change reuses the
   pure single-term streaming matcher in ordinary SQL, preserving full input
   validation and complex-query fallback. Record before/after SQL measurements.
2. Complete the existing qualification matrix. Repair fixture preconditions,
   retain real worker/plan/result assertions, and document failures as observed.
3. Establish repeatable comparisons. `tools/fts_compare.py` checks row identity
   equality for fixed ASCII-term workloads, captures plans and transaction
   latency logs, alternates engine order and repeats samples. It expects the
   G6 fixture, a simple-config expression GIN index, and 20 `rareplanet` rows.
4. Deliver SQL ranking before claiming product parity. The pure BM25 engine
   already exists; the SQL score binding, corpus-statistics lifetime, executor
   fallback, eligible-row top-k and host security/MVCC integration do not.
   Implement exhaustive visible-row scoring first, then validate pruning
   against that oracle under residual filters, joins, RLS, updates and LIMIT.
5. Promote faster counting only after visibility and concurrency gates. Avoid
   text reconstruction and heap visits only where exact posting semantics,
   liveness and fresh VM facts justify them. Preserve ordinary aggregate fallback.
6. Bound maintenance under writes. Measure lock waits, WAL bytes, write p99,
   memory across backends, retained generations, vacuum progress and index growth.
7. Stabilize packaging, upgrade policy and supported deployments; complete
   independent FFI/storage review before a production release.

## TIN feature comparison

PlanetScale's [search documentation](https://planetscale.com/docs/postgres/search)
currently advertises these features. These are vendor product claims, not
measurements of Pin or independent certification of TIN.

| Capability | Pin status / required work |
|---|---|
| Native text index, Boolean search | Implemented; small-workload qualification exists |
| Phrase matching | Implemented; retain exact sequential oracle |
| BM25 and top-k SQL | Pure engine exists; PostgreSQL integration still required |
| Proximity and span composition | Extend query semantics and independent oracle first |
| Fuzzy, regex, general wildcard, boosts | Not feature-equivalent; bound expansion and work |
| Highlighting | Needs original-text offset mapping and escaping contract |
| Cross-column scoring | Needs explicit score binding and combined-statistics contract |
| Accent/emoji tokenization | Current profile differs; versioning/reindex policy required |
| Fast exact counts | Experimental; visibility shortcuts remain gated |
| Parallelism | Implemented paths need lifecycle qualification and measured scaling |
| Search on read replicas | Unsupported pending replay/retention protocol |

The [TINQL reference](https://planetscale.com/docs/postgres/search/tinql) and
[scoring reference](https://planetscale.com/docs/postgres/search/scoring) define
comparison requirements. Similar features do not imply identical SQL or analyzer
semantics. Any parity claim needs an explicit shared test corpus.

The [TIN architecture article](https://planetscale.com/blog/introducing-tin)
points to physical tuple locality, page-group operations and avoiding unnecessary
work as useful design directions. Pin already uses physical tuple identities;
the current measurements show predicate work can dominate the index lookup.
Optimize the measured whole query before adding ISA-specific code. Published
TIN performance figures remain vendor measurements, with no local TIN comparison.

## PostgreSQL obligations

Follow pinned [index AM contracts](https://www.postgresql.org/docs/18/indexam.html),
MVCC, HOT, VACUUM, buffer ownership and WAL lifetimes. Record each changed boundary
in `api-evidence.md`. Source acquisition and testing use the committed toolchain
and Cargo.lock. New unsafe or visibility shortcuts require independent review.

## Next acceptance evidence

Small datasets remain appropriate during development. Expand query diversity
and mutation schedules before increasing volume. Required measurements include
selective/broad terms, AND/OR/NOT, phrases, LIMIT, Unicode, varying document size,
cold/warm reads and concurrent mutation. Capture plans and correctness before
latency; compare identical row identities when analyzer semantics overlap.

The comparison tool is read-only and uses a dedicated database selected by
libpq environment variables. Prepare a fresh database with extension Pin, then
run `psql -X -v ON_ERROR_STOP=1 -f benches/g6/compare_setup.sql`. The setup fails
if its table already exists. Example benchmark invocation:

```sh
PGHOST=/path/to/socket PGPORT=55483 PGDATABASE=pin_bench \
  python3 tools/fts_compare.py --bindir /path/to/pg18/bin \
  --output .artifacts/fts-comparison --samples 3 --seconds 5
```

Do not use lifecycle-test cost overrides as benchmark settings. They select a
specific execution path for worker failure testing and are not cost calibration.
