# Document extent experiments, 2026-09-26

Status: experimental and default off. No merge qualification or TIN parity claim.

## Revisions

- Fixed 3072-byte prefix: `8d44ffb0a77fe8a3470889c7d6b27d357e46db20`.
- Adaptive prefix: `65c5c1e7c9514bed2f8379d9e2aed22551a3906a`.
- Adaptive prefix plus retained tail buffer: `81041007ece01b4ff095c9f4cbc97487056b4fab`.

Each revision compares legacy and mapped storage in the same native binary.
Each uses a separate PostgreSQL 18.6 disposable cluster with 128MB shared buffers,
fsync/full_page_writes on, autovacuum off, and no parallel queries or JIT.
The warm serial fixture has 256 documents, 16000 repeated echo occurrences and
late lexical terms omega/zulu. Four alternating blocks contain 100 queries per
case/mode. Values are median backend scheduler CPU milliseconds per query, not
wall latency or hardware instruction counts. Stored-vector GIN controls, actual
plans, ordered identity oracles, settings, samples, build CPU/WAL and index sizes
are retained in each run directory. No hardware PMU measurements were taken.

## Results

| Run | Late match ms | Repeated match ms | Negative ms | PIN index bytes |
| --- | ---: | ---: | ---: | ---: |
| pin-directory-legacy-late | 0.777 | 0.398 | 1.288 | 4276224 |
| pin-directory-mapped-late | 1.136 | 0.328 | 2.197 | 6373376 |
| pin-adaptive-legacy-late | 0.769 | 0.414 | 1.305 | 4276224 |
| pin-adaptive-mapped-late | 1.219 | 0.412 | 1.825 | 4276224 |
| pin-tailreuse-legacy-late | 0.785 | 0.408 | 1.294 | 4276224 |
| pin-tailreuse-mapped-late | 0.931 | 0.405 | 1.515 | 4276224 |

The adaptive layout removes the fixed-prefix extra page in this fixture. Retaining
the tail scratch improves the mapped reader relative to the previous iteration,
but it still costs more CPU than the same-binary legacy reader on late queries.
Cross-revision differences include run noise; use the controls and raw samples.
The final mapped run remains about 7x stored GIN CPU on the late match. This is
not a successful universal replacement of legacy storage and is kept opt-in.

The 16K fixture needs two document pages with either final layout. Physical skips
save no pages here. The core 60K rare-term fixture proves head plus selected tail
reads rather than a full chain, but that work bound does not establish native
latency. A longer native workload and a CPU profile are needed to separate the
cost of directory validation, virtual reads, copied metadata and kernel work.
GIN tsvector positional limits must be respected when selecting comparable tests.

## Correctness evidence and remaining gates

- Adaptive full core suite passed; a later standalone compatibility test and the
  final 11-test document-directory suite passed.
- Final retained-buffer focused suite: 11 passed; core all-target Clippy clean.
- Native adaptive and retained-buffer lifecycle: 44 identity comparisons each.
- Every allowed document byte length is checked against head capacity, fragment
  count and map bounds; zero-tail, v0 compatibility and unknown-version tests pass.
- Initial fixed-prefix native lifecycle failed because the fixture reused id 7.
  The failure log is retained. The fixture now uses id 99; this was a harness defect.
- Final full-core rerun, native committed/uncommitted crash replay, wider workload
  controls and CPU profiling remain required before merge consideration.

## Reproduction

Build the exact revision with `cargo pgrx package -p pin-pg` and
`PIN_BUILD_REVISION=$(git rev-parse HEAD)`, using PostgreSQL 18 pg_config.
Set PGHOST/PGPORT/PGDATABASE to the disposable cluster that loads that package.

```sh
python3 tools/fragment_phrase_bench.py --disposable --stored-control --late-terms --rows 256 --tokens 16000 --blocks 4 --queries 100 --output /tmp/new-legacy-run
python3 tools/fragment_phrase_bench.py --disposable --stored-control --direct-documents --late-terms --rows 256 --tokens 16000 --blocks 4 --queries 100 --output /tmp/new-mapped-run
python3 tools/phrase_lifecycle.py --direct-documents --output /tmp/new-lifecycle-run
```

No speed or correctness claim here applies to native BM25, fuzzy, wildcard,
proximity/range queries or arbitrary concurrent production workloads.

## Longer documents and crash replay

The follow-up uses the same `81041007ece01b4ff095c9f4cbc97487056b4fab`
binary and 256 documents with 60000 repeated tokens. `--pin-only` excludes GIN:
PostgreSQL's tsvector positions cannot preserve the late adjacency at this length.
See [official positional limits](https://www.postgresql.org/docs/18/textsearch-limitations.html).
Each result is checked against both PIN's scalar scan and fixture-derived IDs
(even rows for the late phrase, all rows for repetition, none for the negative).
Four blocks of 100 unprofiled queries precede separate five-second cpu-clock
samples per case. These layouts were run sequentially, not interleaved.

| Case | Legacy CPU ms | Mapped CPU ms | Outcome |
| --- | ---: | ---: | --- |
| Late rare adjacent phrase | 2.327 | 0.876 | 2.66x faster |
| Repeated early phrase | 0.410 | 0.404 | Approximately unchanged |
| Late rare then dense, no match | 4.212 | 4.874 | 15.7% slower |

Both PIN indexes occupy 16859136 bytes. Build CPU is 2104 ms legacy and
2087 ms mapped; WAL is 15781896 versus 15792032 bytes. These are single builds.
The mapped layout wins when it skips unrelated position pages. It does not yet
win when the query needs the dense stream. The negative-case regression and the
16K regression are unresolved and preclude enabling this by default.

Late-match CPU samples attribute 32.93% of legacy time and 12.38% of mapped time
to memmove. The mapped negative profile attributes 38.24% to the phrase witness,
17.96% to memmove and 7.98% to memset. Sample percentages are statistical
attribution, not exact byte counts, instruction counts or causation proof.
Raw perf recordings (gzip), textual reports, stderr/sample counts, plans and SQL
are in `long-documents/`; SHA256 hashes cover uncompressed recordings and the
matching `pin.so` binary. Restore the library to the capture's absolute package
path for symbol resolution. System binaries are not included. Initial runs failed
before profiling because the new harness omitted a SQL terminator; failure logs
are retained and the corrected runs use the `2` suffix.

Final retained-buffer core tests: 221 passed, 3 existing ignored, zero failures. Native recovery on the same binary:
checkpoint, committed insert, open uncommitted insert, immediate shutdown, WAL
replay, exact bitmap/scalar comparison, VACUUM, insert with creation GUC off,
REINDEX and cleanup all passed. `long-documents/pin-extent-recovery/` contains SQL,
plans, control output and recovery logs. The tool checks the connected data
folder against the explicitly requested `/tmp` cluster before stopping it.
[Immediate shutdown invokes crash recovery](https://www.postgresql.org/docs/18/app-pg-ctl.html).
This is one committed/uncommitted replay scenario, not torn-write, replication,
concurrent VACUUM or exhaustive crash-point qualification. Successful insertion
after VACUUM does not by itself prove physical free-page reuse.
