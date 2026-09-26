# Native dense-position seek and page-copy iteration

PR #28 merged as `357662c8c6e3d36133f45c5c118bbbce7cd2b952`.
Baseline native module: `189a02ddbe3c1c67f1495eba2e4725a1af7ace7d`.
Inline-only module: `f8912e27d28cc3a10158a5c041f39c7328d9d2e9`.
Combined module: `450799eaa2cf0b2214b1aa0cc45bc03d2d95bd77`.
The PB01 codec merged in PR #28 is not in the native path. Its presence does not
account for these results. Later commits add tests/documentation only.

## What changed

Three page-return wrappers now request inlining, enabling the compiler to
eliminate some transfers of large private Page values. This is a code-generation
hint, not a new buffer lifetime or zero-copy PG pointer. Every page retains its
private bytes and previous locks/validation. The baseline profile motivated this
ablation; memmove was 40.56% of flat samples.

Phrase seek now recognizes runs of 32 canonical delta-one bytes below its target.
It advances their positions together instead of visiting every occurrence. It
still charges 32 units of occurrence work, checks token bounds, and verifies the
end of a consumed complete stream. Sparse/general deltas retain scalar decoding.
This specifically benefits late targets in dense occurrence lists; it does not
supply arbitrary random access to linked fragments.

## Measured native CPU

Warm serial bitmap count; PostgreSQL 18.6; exact SQL and plans retained. PIN and
GIN samples alternate inside each run. GIN uses a stored generated tsvector,
not expression-index heap text reanalysis. Long fixture: 256 documents with
10,000 repeated tokens. Initial baseline/inline use four blocks; repeat baseline
and combined builds use six. Every block contains 100 timed queries. Each result
is median backend scheduler CPU per query, not client latency or hardware cycles.

| Long phrase | Baseline run 1 / repeat ms | Inline only ms | Combined run 1 / repeat ms | Combined GIN run 1 / repeat ms |
| --- | ---: | ---: | ---: | ---: |
| alpha beta | 0.929 / 1.090 | 0.709 | 0.622 / 0.870 | 0.113 / 0.211 |
| echo echo | 0.915 / 1.093 | 0.739 | 0.627 / 0.875 | 0.134 / 0.234 |
| alpha echo, no match | 10.154 / 17.729 | 12.578 | 1.551 / 2.229 | 0.203 / 0.352 |

The negative case improves approximately 6.5x and 8.0x when pairing the first and
repeat invocations respectively. Matching cases improve about 1.25–1.49x on
those pairings. Builds were not interleaved in a single process; VM conditions
changed substantially, including the GIN controls. These ratios are observations,
not a precision estimate of the isolated code effect. The inline-only negative
result regressed in absolute CPU; this unsuccessful intermediate result is kept.
Some build/test commands overlapped early measurements; no production p99 or
isolated-machine claim is justified.

Short control: 2,048 documents, 32 repeated tokens, four blocks x 100 queries.

| Phrase | Baseline PIN ms | Combined PIN ms | Combined stored GIN ms |
| --- | ---: | ---: | ---: |
| alpha beta | 1.355 | 1.120 | 1.007 |
| echo echo | 1.205 | 1.028 | 1.063 |
| alpha echo | 1.725 | 1.661 | 1.165 |

The short repeated result is approximately tied with GIN; a 3% difference is
insufficient to claim a robust win. The other short cases still lose. Long
combined PIN remains roughly 3.7–7.6x GIN CPU. This is progress toward the target,
not 10x better than GIN, TIN parity, or production qualification.

## Profile and correctness

Combined adjacent-phrase cpu-clock profile: 1,572 samples, zero lost samples; memmove is 30.92%
of flat samples, compared with 40.56% previously. Sample shares are not absolute
bytes copied or proof that every removed copy came from a particular wrapper.
The profile executed 10,450 queries in its window; profiling perturbs execution.
Raw perf.data, report, symbol-bearing module, disassembly and profiler script are
included. The captured x86-64 disassembly uses two 16-byte `pcmpeqb` operations
and a mask check for the dense-run equality test, generated from safe Rust.
Hardware cycles/instructions remain unavailable. Module text grows
from 4,349,664 to 4,417,828 bytes, approximately 1.6%.

Every benchmark checked ordered id/ctid identities against the scalar oracle and
GIN before timing, and retained actual plans. Twenty query oracle tests pass.
Two new dense-seek unit tests cover boundary targets, budgets, every corrupt byte
position, token overflow and trailing data. Native lifecycle: 33 comparisons
passed, including HOT/indexed updates, own writes, rollback, delete, VACUUM,
REINDEX and an old repeatable-read snapshot. Clippy is clean. Full core suite: 208 passed, 0 failed, 3 existing ignored; the raw test log is included.

No format, WAL, visibility, FFI, or unsafe changes. The phrase path remains
opt-in. Skipped unused tails retain the previously documented integrity policy.
These tests do not constitute a new crash-recovery campaign or concurrency proof.

## Reproduction and remaining work

Build each pinned source with the installed PG18/pgrx toolchain and explicit
PIN_BUILD_REVISION, then initialize separate C.UTF-8 disposable clusters with
absolute package paths. Connection parameters use PGHOST/PGPORT/PGDATABASE.

```sh
python3 tools/fragment_phrase_bench.py --disposable --stored-control --blocks 6 --queries 100 --output /tmp/new-long-run
python3 tools/fragment_phrase_bench.py --disposable --stored-control --rows 2048 --tokens 32 --blocks 4 --queries 100 --output /tmp/new-short-run
python3 tools/phrase_lifecycle.py --output /tmp/new-lifecycle
cargo test -p pin-core
cargo clippy -p pin-core --all-targets -- -D warnings
```

Next: direct physical positional extents and reusable page buffers with explicit
budget/lifetime accounting. PB01 must eventually avoid linked-fragment copying,
not just decode a copied payload faster. Native BM25/top-k and TIN feature parity
remain separate missing capabilities. No live TIN benchmark has been run.

The failed initial baseline connection (missing PGDATABASE) is retained. Test
clusters are disposable; the system server is untouched. RAW_SHA256.txt covers
all files under raw, including compressed tested modules.
