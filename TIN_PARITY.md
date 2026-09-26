# TIN parity target and PIN status

The performance target is TIN-like indexed CPU and latency for each query
class, with matching user-visible search semantics and PostgreSQL lifecycle
correctness. It is not met. The public [TIN architecture](https://planetscale.com/blog/introducing-tin)
uses CTID-oriented page/offset bitmap postings, selective positional data,
exact term counts, visibility-aware count execution, and immutable segments.
The current PIN index still has a canonical owner/dictionary layer and a
separate grouped layer. The opt-in packed dictionary only removes the
two-document posting-page pathology.

The current [TINQL specification](https://planetscale.com/docs/postgres/search/tinql)
defines implicit **AND**, not implicit OR. Use the published specification as
the compatibility target if an older demonstration says otherwise.

| User capability | PIN status on main or this draft | Parity requirement |
| --- | --- | --- |
| Boolean terms and simple exact phrases | Implemented with PIN syntax; index proof varies by plan | TINQL syntax, precedence, nested expression semantics |
| Prefix search | Implemented as a limited PIN query | Both `*` and `?` wildcards, term ranges, regex |
| Fuzzy edit distance | Missing | Stable-prefix and distance forms |
| Phrase gaps and alternatives | Missing | One-word gaps, per-position alternatives, tolerance |
| Proximity, span relations and position filters | Missing | `THEN`, `NEAR`, `WITHIN`, containment, overlap, first/last/middle/range |
| Boosts and scored top-k | Pure scoring code exists; SQL BM25 context and native top-k path missing | Correct `score`/`max_score`, score context, pruning with exact totals |
| Highlighting | Missing | HTML and ANSI highlight with explicit and bound query forms |
| `COUNT(*)` | Experimental guarded fast paths, general host count remains | Exact efficient counts with MVCC and visibility-map handling |
| ACID, updates, VACUUM, replay | Bounded tests; packed mode experimental | Full concurrent and restart qualification |

PlanetScale publishes [Lead](https://github.com/planetscale/lead) as an open
source, deliberately slow implementation with TIN-compatible SQL, tokenizer,
TINQL, BM25 and highlighting behavior. It is a useful small-corpus semantic
oracle. Its AGPL-3.0 source must be handled as separately licensed material;
this repository does not incorporate Lead code.

`pin_next.md` remains the implementation map: finish A1 packed canonical
arenas, A2 SQL BM25, then B1 authoritative packed CTID postings with separate
frequency/position streams and a direct bulk builder. The draft packed-page
step reduces storage but does not establish read parity or complete A1.
