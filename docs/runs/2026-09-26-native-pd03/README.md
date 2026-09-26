# Native PD03 qualification, 2026-09-26

This is a qualification run for the experimental persisted blocked-position
capability in draft PR #30. It is **not** a production performance claim.
`pin.enable_blocked_positions` defaults off. The raw per-query samples, plans,
SQL, settings, build/WAL totals, recovery output, and software `perf` profiles
are in the directories here. `compressed-manifest.json` records hashes for
the three exact native binaries and compressed `perf.data` files.

## Paired CPU results

Each pair uses the same binary, warm serial execution, 256 rows, and alternating
query blocks. Values are median backend scheduler CPU in milliseconds/query.

| Binary, workload | Format | Adjacent | Repeated | Negative | PIN index bytes | Build CPU ms | Build WAL bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `2bbd3c5`, 60,000 repeated tokens/row | PD02 mapped | 0.874 | 0.432 | 3.625 | 16,859,136 | 2,087 | 15,808,360 |
| `2bbd3c5`, 60,000 repeated tokens/row | PD03 blocked | 1.243 | 1.554 | 1.569 | 18,956,288 | 2,396 | 17,630,400 |
| `7d04b9b`, 32 tokens/row | PD02 mapped | 0.154 | 0.132 | 0.174 | 106,496 | 20 | 865,808 |
| `7d04b9b`, 32 tokens/row | blocked capability, PD02 payload | 0.154 | 0.130 | 0.169 | 106,496 | 17 | 865,784 |
| `214a6ea`, 16,000 repeated tokens/row | PD02 mapped | 0.987 | 0.430 | 1.172 | 4,276,224 | 559 | 4,310,088 |
| `214a6ea`, 16,000 repeated tokens/row | PD03 blocked | 1.243 | 1.142 | 1.275 | 6,373,376 | 1,071 | 4,823,608 |

On the final 60,000-token pair, PD03 uses 2.31 times less CPU on the negative
case, but 1.42 times more on adjacent and 3.60 times more on repeated. It also
uses 12.4% more index space and about 15% more build CPU. The earlier 16,000
token pair includes a comparable stored-vector GIN control: its medians were
0.129, 0.132, and 0.200 ms, respectively. PIN is still materially behind GIN
on that corpus. The 60,000-token case has no equivalent GIN control because a
`tsvector` cannot retain all those word positions; it must not be used for a
GIN ratio. The 32-token pair confirms that retaining PD02 for documents without
block candidates removes the initial large regression on short documents.

The `perf` CPU-clock profiles are sampling evidence, not exclusive function
costs. The initial 60,000-token PD03 runs attribute about 23–24% of samples in
the negative/repeated cases to `PositionDirectory::open` and about 24–27% to
`memmove`. That points to repeated directory validation and byte movement as
costs to investigate; it does not prove either is entirely removable. PMU
counters were unavailable on this GCP VM.

## Correctness and limits

The final core suite passed with no failures (`pin-pd03-final-core.log`), and
the PostgreSQL adapter passed Clippy. Native lifecycle checks compared 44
identities across build, HOT/indexed updates, delete, VACUUM, concurrent
visibility, and REINDEX. Committed and uncommitted immediate-crash replay
checks passed in a PostgreSQL 18.6 cluster with fsync and full-page writes on.
These are bounded fixtures, not exhaustive production qualification.

The current representation is not the primary packed index described in
`pin_next.md` A1/B1. PD03 is a selected-position experiment. The next
architecture gate is to remove sparse canonical posting-page overhead and
measure both read and write CPU, WAL, page census, and correctness on singleton,
two-document, and dense-term corpora. Ranked SQL BM25 remains an independent
feature gate (A2). Do not promote PD03 or merge this draft based on its single
winning negative case.
