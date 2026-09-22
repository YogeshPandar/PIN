# Owner-aware direct-page seek checkpoint

Code: `eaad9c22a5ab47d1fc64a7fa718792183bdd08d8`; preceding installed
binary for `before` was `2dbae648e2f0af0aa7f52d1f82e769de09ab264d`.
The page-range change skips a second decode of a validated direct posting page
when its final owner precedes the AND seek target. Canonical owner identity
and incarnation checks remain. A preceding commit deferred reading copied
TIDs until Boolean survivors were known.

`before` and `after` each contain three alternating three-second samples of
`alpha AND rareplanet`. `full` repeats all six fixed Pin/GIN queries against
the final binary. The 20,000-row synthetic fixture, PostgreSQL 18.6 with
assertions, four-vCPU VM, warm tmpfs, four clients, two threads, serial bitmap
plans, JIT off and Pin count/VM shortcuts off match the earlier run. The
`environment.json`, SQL, plans, symmetric identity checks and raw sample JSON
are archived. The source revision embedded in the server is recorded there.

| Query | Before Pin / GIN median QPS | After Pin / GIN median QPS |
| --- | ---: | ---: |
| selective AND | 4,462.78 / 29,501.15 | 7,022.44 / 31,109.65 |

The full rerun measured 7,030.17 / 30,422.87 selective AND QPS. The full
comparison and practical limits are explained in
[PERFORMANCE_REVIEW.md](../../../PERFORMANCE_REVIEW.md). This is no TIN
comparison, and the fixture is too small and hot for a production claim.

Validation: focused direct and Boolean owner-oracle tests, full release pure
Rust suite, warning-free release Clippy, 63 Python tests, contract check, and
full test-hooks G2 PostgreSQL qualification, normal-package G8 (234/234), and
hard postmaster WAL recovery with indexed identity checks passed. The recovery
fixture retained four direct pages out of 51 total pages after restart. A new
focused pure test puts the rare match after multiple direct pages. The earlier
[direct-segment run](../2026-09-22-direct-segments/README.md) covers the
unchanged page format and WAL protocol.

Reproduce with `tools/fts_compare.py --bindir PATH --output PATH --samples 3
--seconds 3 --exact-bitmap on --cases selective_and` against the G6 fixture.
Omit `--cases` for the full run. Row identity is checked before timing.
