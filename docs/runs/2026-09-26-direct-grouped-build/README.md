# Direct grouped build experiment, 2026-09-26

`pin.enable_direct_grouped_build` is a default-off, superuser setting for
sequential `CREATE INDEX`. It emits grouped sort records during the PostgreSQL
heap build scan and skips the later canonical posting-chain capture. The
canonical index and grouped disk format remain unchanged. Frontier anchors or
insufficient sort memory use the old build path.

## Qualification

The pure core oracle produced byte-identical index pages and equal build
statistics for old and direct grouped builds with 0, 1, and 300 documents,
with packed postings both off and on. All 21 existing grouped core tests passed.
The PostgreSQL 18.6 adapter compiled and passed Clippy with warnings denied.

The native runner created disposable PostgreSQL 18.6 clusters, indexed two
copies of 12,000 synthetic rows, compared nine queries, then compared four
queries after insert, update, delete, and VACUUM. Both indexes were 9,658,368
bytes. The runner and complete stdout/stderr are in `tools/` and `raw/`.

| Build order | Old build CPU | Direct build CPU | Direct change |
| --- | ---: | ---: | ---: |
| Old then direct | 1,120.07 ms | 1,129.60 ms | 0.9% higher |
| Direct then old | 1,133.69 ms | 1,105.66 ms | 2.5% lower |

CPU is backend `/proc/self/schedstat` time around `CREATE INDEX`. The results
change sign with build order, so this experiment establishes no build speedup.
The direct-path debug message confirms that the candidate path actually ran.
Query execution is unchanged and was not benchmarked by this experiment. It
does not close the gap to TIN or establish production readiness.

The result shows why removing an extra build traversal is only a preparation
step. Ordinary insertion still writes canonical posting chains, and grouped
search still resolves canonical term identities and mutable suffixes. The
next performance change must address the primary query representation and
ranked/positional execution described in `pin_next.md`, with native CPU
qualification for each workload.
