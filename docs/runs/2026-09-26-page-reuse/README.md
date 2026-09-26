# Native private page reuse

Tested module: `2fbeab720ace89b71e9133058960dce19cbeb142`.
Baseline module: `450799eaa2cf0b2214b1aa0cc45bc03d2d95bd77` (the preceding
native dense-seek iteration). The intermediate `50d6223` added in-place reload;
`2fbeab7` also retains one budgeted fragment image across candidate documents.

## Connection to pin_next.md

[A3 in the plan](../../../pin_next.md#133-the-first-implementation-tranche)
requires selected-position interfaces and adapters over inline/fragmented legacy
storage before adopting the packed codec. This iteration advances that reader
work. Sections 4.4 and 4.5 still set the destination: independently addressable
sub-blocks with membership, frequency and positions read only when needed.
The new buffer API does not complete that physical format, A1 packed postings,
or A2 native SQL BM25. The measured copy profile supplied the immediate priority;
the plan supplied architectural boundaries and the broader roadmap.

## Implementation and correctness

- `Page::reload_with` reuses its exclusively borrowed array. All metadata and
  bytes are reset, preserving fresh-image zero-tail semantics. Returned errors
  invalidate the block identity and expose no payload. A later valid reload can
  recover that private object.
- `PageStore::read_into` supplies a compatibility default; PgStore copies directly
  into the supplied image through its existing guarded C call.
- Owner caches reload in place. Phrase traversal retains one fragment image
  across documents and reloads it after consuming the current payload. Its size
  is charged to the remaining query budget; insufficient budget keeps heap
  recheck. No extra heap allocation is introduced.
- The original host-to-private copy remains under a PG shared content lock.
  No host page pointer enters the engine, and no buffer pin lifetime changes.
  Kind/header/publication/incarnation checks remain. The structural barrier,
  heap visibility, WAL and on-disk PD02 format remain unchanged.

This removes redundant transfers of large Page values. It does not remove PD02
payload assembly or provide random physical access to positional blocks.
Zero-fill is intentionally retained; removing it needs separate mutation and
stale-tail qualification.

## Native backend CPU

Same synthetic recipe for both builds. Warm serial bitmap counts, PG18.6,
C.UTF-8, fsync/full_page_writes on, 128 MB shared buffers. Each run alternates PIN
and stored-vector GIN for six blocks of 100 queries. Full ordered id/ctid oracle
checks and actual plans are saved. Builds ran serially in this order:
baseline long, reuse long, baseline short, reuse short, reuse long repeat,
baseline long repeat. Build and core-test processes finished before timing.

Long corpus: 256 documents, 10,000 repeated tokens. Median scheduler CPU ms/query:

| Phrase | Baseline first / repeat | Reuse first / repeat | Reuse stored GIN first / repeat |
| --- | ---: | ---: | ---: |
| alpha beta | 0.753 / 0.660 | 0.437 / 0.429 | 0.124 / 0.126 |
| echo echo | 0.731 / 0.632 | 0.417 / 0.419 | 0.122 / 0.130 |
| alpha echo, no match | 1.652 / 1.559 | 1.066 / 1.101 | 0.195 / 0.210 |

Observed CPU reduction is approximately 29–43% across these paired cases/runs.
These are cross-build measurements on a shared VM, not an isolated same-binary
ablation. Long PIN remains approximately 3.2–5.5x the stored-vector GIN CPU.

Short corpus: 2,048 documents, 32 repeated tokens:

| Phrase | Baseline PIN | Reuse PIN | Reuse stored GIN |
| --- | ---: | ---: | ---: |
| alpha beta | 0.712 | 0.641 | 0.611 |
| echo echo | 0.639 | 0.600 | 0.618 |
| alpha echo | 0.974 | 0.925 | 0.694 |

The short repeated case is approximately tied with GIN, not proof of a robust
3% win. The other short cases still lose. No TIN service was tested. No claim of
10x-over-GIN performance, production p99, or complete feature parity follows.

## Profile and validation

Adjacent-phrase profile: 1,533 cpu-clock samples, zero lost samples. Memmove is
15.39% of flat samples, versus 30.92% in the preceding iteration. Memset remains
4.11%. This is a sample-share comparison, not a count of transferred bytes.
The profile also shows relation-size/lseek work; future caching must prove its
relationship to concurrent growth and PG relation lifetime before removing bounds
checks. Hardware cycles/instructions remain unavailable on this VM.

- Core suite: 210 passed, zero failed, three existing ignored tests.
- Focused query/image suite: 20 query oracles and two new image tests passed.
- All-target core Clippy: clean with warnings denied.
- Native lifecycle: all 33 comparisons passed, including writes, rollback, HOT,
  delete, VACUUM, REINDEX and an older repeatable-read snapshot.
- The image tests check stable address, multiple page kinds, zero tails,
  callback/header/length failures, invalidation and recovery. Query test stores
  exercise in-place reload as well as the compatible trait default elsewhere.

No new crash/replay or multiclient throughput campaign was performed. This is
an incremental optimization under the existing experimental feature contracts.

## Reproduction and artifacts

Build the exact module revision with PIN_BUILD_REVISION and the installed PG18
pgrx toolchain. Initialize a separate disposable C.UTF-8 cluster and set the
absolute package paths shown in raw/postgresql.conf. The captured orchestration
script `raw/pin-page-reuse-run.py` identifies both clusters, run order and CLI
arguments. It assumes those binaries/clusters have already been prepared.

```sh
cargo test -p pin-core
cargo clippy -p pin-core --all-targets -- -D warnings
PGHOST=/tmp/pin-reuse-final-socket PGPORT=55498 PGDATABASE=postgres \
  python3 tools/fragment_phrase_bench.py --disposable --stored-control \
  --blocks 6 --queries 100 --output /tmp/new-reuse-run
```

Raw SQL, plans, settings, scheduler samples, result identities, logs, perf.data,
profile script and symbol-bearing tested module are included. build.json records
the module hash and platform. Both disposable servers were stopped after the
run; the system server was untouched. RAW_SHA256.txt covers every raw artifact.
