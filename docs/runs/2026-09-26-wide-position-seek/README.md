# Wider dense positional seeks

Source: `604dc8026f5408fc9d29221f1d3f229a587da46f`.
Prior source: `81041007ece01b4ff095c9f4cbc97487056b4fab`.
Prior measurements: [document extent experiments](../2026-09-26-document-extents/README.md).

## Mechanism and correctness

The scalar counted-position decoder remains the fallback. A checked 256-byte
all-ones comparison now precedes the existing 32-byte dense-run comparison.
A skip is admitted only after the first absolute position, with enough remaining
occurrences, bytes and work budget, and only below the target position. It verifies
all consumed deltas are canonical +1, checks token bounds, charges work per
occurrence and validates trailing bytes on stream exhaustion. No new on-disk
format, unsafe operation, PostgreSQL buffer or WAL contract is introduced.

A scalar-reference differential covers dense/mixed runs, target boundaries and
budgets from zero through exhaustion; malformed deltas at each of 512 byte
locations are rejected. The focused three-test seek suite and all-target Clippy
pass. Native lifecycle: 44 identity comparisons pass. Full core suite: 222 passed,
3 existing ignored, zero failures.

## Native measurements

Same 256-document fixture and warm serial backend scheduler CPU methodology as
the prior extent runs. The 60K-token workload omits GIN because its positional
semantics cannot represent these late phrases. The 16K and 32-token fixtures keep
stored-vector GIN controls. Four blocks of 100 queries are used for long fixtures;
short controls use six blocks of 200. No profiling was active during timings.
Both scalar and fixture-derived ordered IDs match the index results.

| 60K case and layout | Previous CPU ms | Wider skip CPU ms |
| --- | ---: | ---: |
| Late rare match, legacy | 2.327 | 2.285 |
| Late rare match, mapped | 0.876 | 0.863 |
| Negative phrase, legacy | 4.212 | 2.881 |
| Negative phrase, mapped | 4.874 | 3.544 |
| Repeated early match, legacy | 0.410 | 0.419 |
| Repeated early match, mapped | 0.404 | 0.425 |

The negative case improves about 32% for legacy and 27% for mapped storage.
Mapped negative queries remain 23% slower than same-binary legacy queries.
The wider comparison does not eliminate copying or physical reads. Cross-revision
values come from sequential runs, so small differences are not established gains.
Repeated early mapped CPU is 5.3% higher in this run and remains recorded.

At 16K, mapped negative CPU falls from 1.515 to 1.131 ms; stored GIN in the new
run uses 0.217 ms. Mapped rare match uses 0.940 ms versus GIN 0.128 ms. This is
still far from the requested performance and does not qualify default enablement.

Short controls (32 tokens), old -> new PIN CPU microseconds:

- Adjacent: 145.6 -> 149.4 (+2.6%).
- Repeated: 134.3 -> 136.9 (+2.0%).
- Negative: 167.0 -> 158.4 (-5.1%).

GIN controls vary too; the results do not establish a broad short-query speedup.
Index size and write layout are unchanged. Raw samples, plans, build/WAL metrics,
settings, SQL, identities and test logs are retained alongside this report.

## Remaining architecture work

This improves scans through dense delta bytes but still examines those bytes.
The next positional representation must let the reader locate a relevant block
or prove it cannot contain a witness using checked block bounds, without fetching
and copying the entire dense stream. Existing experimental PB01 codec groundwork
is not yet native storage. Integrating it needs format discrimination, legacy
readers, all payload consumers, budget accounting, full integrity validation and
publication/VACUUM/replay qualification. The broader packed-primary storage and
native ranked/BM25 work in pin_next.md remain outstanding.
