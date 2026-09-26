# Prefix witness iteration

Final tested module source: `189a02ddbe3c1c67f1495eba2e4725a1af7ace7d`.
Initial prefix source: `3b312037df72a3824ad55434f66d603f81b9842d`.
Prior reader source: `66c6c1e86cb46d59b5544e15b0e63ab8f2b78a06`.

The reader now stops at a checked phrase witness and can avoid reading the tail
of a fragmented document. An incomplete selected stream gets at most 256 cursor
advances in the initial probe, then the existing bounded full reader takes over.
Complete selected streams finish without fetching unrelated tails. There is no
new format, WAL operation, unsafe code or visibility shortcut.

## Native CPU results

Same synthetic fixture recipe, 256 documents with 10,000 repeated tokens,
PostgreSQL 18.6, C.UTF-8, warm shared buffers, serial execution. Six alternating
batches of 100 queries for PIN and GIN on a stored generated tsvector. The old/new
PIN ratio is cross-build, not a same-binary toggle. Both measurements' raw data
and individual GIN controls are retained.

| Phrase | Previous PIN | Final PIN | PIN improvement | Final stored GIN |
| --- | ---: | ---: | ---: | ---: |
| alpha beta | 1.696 ms | 0.888 ms | 1.91x | 0.112 ms |
| echo echo | 16.831 ms | 0.869 ms | 19.36x | 0.117 ms |
| alpha echo (no matches) | 24.103 ms | 9.873 ms | 2.44x | 0.214 ms |

The initial unbounded prefix probe measured 18.730 ms for the no-match case.
Capping duplicate probe work brought that down to 9.873 ms on the final binary.
The matching repeated phrase no longer decodes the full 10,000-occurrence list.

Short-document control: 2,048 documents with 32 repeated tokens, four alternating
batches of 100 queries:

| Phrase | Final PIN | Stored GIN | PIN / GIN CPU |
| --- | ---: | ---: | ---: |
| alpha beta | 0.943 ms | 0.597 ms | 1.58x |
| echo echo | 0.756 ms | 0.618 ms | 1.22x |
| alpha echo | 1.176 ms | 0.713 ms | 1.65x |

PIN remains slower than stored-vector GIN on every case. These gains do not
establish TIN parity or 10x over GIN. Late/negative queries still expose the cost
of the owner/document layout and sequential positional deltas. A directly
addressable position-block format remains necessary. This synthetic repetition
fixture is a mechanism test, not a realistic corpus or production tail-latency
qualification.

## Correctness and actual work

All 20 query tests passed, including every byte prefix of 120 generated
query/document combinations checked against the independent text oracle.
Physical I/O assertions prove one fragment read for early witnesses and more
than one for late terms. Consumed corruption, unused-tail integrity behavior,
repetition, phrase order, scan budgets, and incomplete-probe budgets are covered.
Core Clippy passed with warnings denied. All 33 native lifecycle comparisons
passed, including old snapshots, HOT/indexed updates, rollback, VACUUM and REINDEX.
All benchmark modes' complete ordered id/ctid streams match the scalar oracle.
The preceding iteration's 197-test full-core run is not represented as a new
full-suite run for this source.

Query corruption detection is intentionally local to consumed headers/deltas.
Unused selected-stream tails, directory tails and fragment tails may remain
unread after a proof. Full document/chain validation is still required for an
integrity claim. A query proof never substitutes for PostgreSQL heap visibility.

## CPU profile

The final adjacent-phrase profile captured 1,701 software-clock samples, zero
lost, while 9,905 queries ran. Flat/self samples attribute 40.56% to memmove and
6.11% to page loading. Some stack unwinding is incomplete; this is not an exact
copy-byte attribution. Profiling ran separately from timed batches. The VM does
not support the requested cycles/instructions events, so there is no measured
IPC, hardware cycle or cache-miss claim.

Raw artifacts include SQL, plans, settings, full identities, schedstat snapshots,
client timings, software profile and tested module with symbols. Decompress
pin.so.gz and compare its SHA-256 with build.json. The ledger covers all raw
files. Prior-run reproduction instructions apply with this source/module and
fresh disposable cluster paths. Run:

```sh
python3 tools/phrase_lifecycle.py --output /tmp/prefix-lifecycle-new
python3 tools/fragment_phrase_bench.py --disposable --stored-control --queries 100 --output /tmp/prefix-long-new
python3 tools/fragment_phrase_bench.py --disposable --stored-control --queries 100 --rows 2048 --tokens 32 --blocks 4 --output /tmp/prefix-short-new
```

No TIN engine was run. [TIN reuse notes](../../tin-reuse.md) distinguish its
published architecture, public Lead feature reference, and private performance
implementation. This iteration independently implements the witness reader.
