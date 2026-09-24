# Issue 14: selective grouped scans and attributable measurements

Baseline: `1d80b58e0eb17b003326283f8050ac3556f80776`, PostgreSQL 18.6.
This work addresses [issue 14](https://github.com/YogeshPandar/PIN/issues/14).
It does not close the issue or establish a 10x result.

## Evidence and implementation

The issue's small warm fixture shows fewer page visits for broad grouped scans,
but worse sparse-term throughput and an unbounded recheck candidate set as the
write delta grows. Those observations are not isolated CPU measurements.

Source inspection identifies avoidable work in the current implementation:

- The snapshot enumerator chooses an AND cover by the number of query terms.
  Equal-size covers follow query order, not the next possible matching group.
- Every positive group performs a fresh liveness catalog lookup, discarding the
  previous leaf, even though output advances monotonically in heap order.
- Bitmap demand walks every candidate page and every query node before the
  actual offset evaluator walks those pages again.
- Single-term queries allocate grouped scratch and consult the supplemental
  catalog even when the canonical posting fits inline or in one small page.
- Offset masks are assembled one byte at a time rather than one little-endian
  machine word at a time.

The implementation uses monotone Boolean lower bounds for group seeks,
reuses the liveness cursor, propagates demanded page masks backwards once, and
retains a bounded canonical sparse path. No disk-format change, borrowed host
buffer, visibility shortcut or new unsafe operation is needed for these changes.
Small-posting selection is a cost hypothesis, not a universal speed guarantee.

## Contracts and primary references

PostgreSQL's [index scanning contract](https://www.postgresql.org/docs/18/index-scanning.html)
requires complete candidate coverage and exact membership whenever recheck is
false. Heap visibility remains PostgreSQL's responsibility. Bitmap counts are
candidate accounting, not visible SQL counts.

The pinned [GIN scan implementation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/gin/ginget.c)
separates scan-key consistency, advancing posting streams and bitmap emission.
The pinned [TIDBitmap implementation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/nodes/tidbitmap.c)
retains per-page recheck/lossiness semantics. This work must not substitute an
index-only timer or popcount for complete query latency or MVCC-visible count.

Rust's [`u64::from_le_bytes`](https://doc.rust-lang.org/std/primitive.u64.html#method.from_le_bytes)
and [`slice::as_chunks`](https://doc.rust-lang.org/std/primitive.slice.html#method.as_chunks)
provide alignment-independent word decoding with an explicit bounded tail.
The workspace toolchain and Cargo-generated lockfile remain unchanged.

PlanetScale's [TIN introduction](https://planetscale.com/blog/introducing-tin)
and [search-engine anatomy](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine)
are design references for avoiding postings work, not specifications or measured
Pin results. TIN's published ranked benchmark is not an equivalent GIN BM25 test.

## Algorithm and regression obligations

A term supplies its next catalog group at or after the requested position. AND
uses the larger child lower bound, OR uses the smaller available child bound,
and NOT supplies the requested position because it cannot exclude a group from
its child's absence. Unbounded roots also seek the liveness catalog. Iteration
stops only at a common conservative bound or exhaustion. Negated descendants
still participate in offset evaluation; they never drive unsafe group skipping.
The tests include both AND orders, holes, nested negation and the final heap group.

The sparse policy accepts only a complete single-term predicate whose captured
canonical chain is inline or one posting page with at most 64 records, including
inline and dead records. It reuses the dictionary and checked head page instead
of repeating lookup or allocating grouped scratch. Mutable, compressed and direct
posting formats all retain their publication/incarnation/liveness checks. New
term writes are read through the canonical chain; unrelated delta owners need not
be emitted. The shared structural barrier and captured-tail contract are unchanged.
The 64-record threshold is deliberately bounded but still needs workload tuning.

Deterministic work tests require one liveness payload and two term payloads for
an AND whose only common group is the last of 256 groups, in both operand orders.
A 130-group broad scan must reuse catalog leaves. Eligible sparse scans must read
zero grouped pages and exactly as many total pages as the canonical scan. These
are regression bounds, not before/after latency results or evidence of 10x GIN.

The byte decoder tests cover every offset width through both 291 and 512 offsets,
both bitmap kinds and unaligned record storage. Backward mask demand is compared
with the previous page-by-page dependency oracle over deterministic mixed programs.
G9 CI explicitly runs these library tests in debug and release. The PostgreSQL
fault/concurrency driver uses nonsparse Boolean queries so stage 37 still proves
actual grouped selection; a separate probe requires sparse fallback to skip it.

## Paired warm profiling

`tools/g9_profile.py` profiles the existing `public.pin_g6_bench` fixture with its
PIN and GIN indexes. The normal disposable G9 runner also creates a 1,024-row
fixture and runs a small protocol smoke test. Those CI samples do not qualify
performance: the machine is shared and two queries per batch are too short for
meaningful CPU-tick or tail-latency evidence.

On a disposable, quiescent PostgreSQL 18.6 installation, create the G6 fixture and
both indexes first. Publish a grouped snapshot before profiling, for example:

```sql
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) public.pin_g6_bench;
```

Then run a normal, non-test-hook build on an isolated database host:

```sh
PGHOST=/path/to/socket PGPORT=5432 PGDATABASE=pin_bench \
python3 tools/g9_profile.py --bindir /path/to/pg18.6/bin \
  --output /new/results/directory --samples 12 --queries 1000 \
  --backend-proc /proc --host-note 'record CPU, affinity, isolation and build flags'
```

Omit `--backend-proc` when not running in the database host's PID namespace.
It is an explicit host assertion, not a remote CPU discovery mechanism. Preserve
hardware details, exact build identity, fixture size/distribution, index sizes,
maintenance state and the entire output directory with each comparison.

The profiler imports one exported repeatable-read snapshot into persistent psql
sessions for grouped-enabled PIN, legacy PIN, GIN and the heap oracle. It compares
full ordered CTIDs in bounded cursor batches, not only counts or hashes. It
rejects unexpected count/bitmap plans, disables custom count shortcuts and keeps
heap visibility/recheck work. Cases include common, rare, selective AND, OR, NOT,
phrase, prefix and absent terms. Enabling grouped scans does not prove that a
particular query used them; sparse/phrase/prefix fallback is intentional.

Every six sample batches cover all mode permutations. Prepared-query client
round trips exclude connection startup but still include protocol/client overhead.
Optional Linux backend user/system ticks and faults are measured around query
batches, with PID-start identity and monotonicity checks. Missing counters are
null with a reason; short batches receive a CPU-resolution warning. RSS is not
an allocation profile. Separate timing-on and timing-off EXPLAIN probes retain
index/heap/buffer work without double-counting inclusive parent counters or mixing
instrumented execution time into the uninstrumented client samples.

Raw samples, plans, SQL, settings, build identity and incomplete/failed status
survive errors. The driver is read-only but its retained snapshot can delay
vacuum; it must not run against an unattended production workload. It does not
reset shared statistics, evict caches or change server-wide configuration.

The additional contracts are PostgreSQL's
[snapshot import rules](https://www.postgresql.org/docs/18/sql-set-transaction.html),
[EXPLAIN instrumentation](https://www.postgresql.org/docs/18/sql-explain.html),
[psql protocol controls](https://www.postgresql.org/docs/18/app-psql.html), and
[Linux proc task counters](https://docs.kernel.org/filesystems/proc.html).
The engine's existing immutable upstream sources and host obligations remain in
[API evidence](api-evidence.md); this change does not add a PostgreSQL FFI boundary.

## Observed validation before this checkpoint

The local suite passed 87 Python/C tests, source contracts, shell syntax and diff
whitespace checks. No Rust toolchain was installed locally. CI for `0b8496b` passed
all core tests and the debug/release grouped tests, including the new read-work and
oracle tests. That checkpoint failed formatting and unused-helper lint checks;
this checkpoint applies the reported formatting and removes the unused helper.
The new profiler's real PostgreSQL smoke test is scheduled by the normal G9 runner,
not claimed as already passed here. Check the exact PR-head workflow results before
merging; earlier successful jobs do not validate later commits.

## Remaining architectural gates

The owner-ordered delta still emits newer live owners with recheck required.
Filtering it efficiently needs a term-addressable append frontier or another
measured delta design, not an unsafe subtraction of global term sets. Phrase and
prefix qualification, full-rebuild writer stalls, sustained-write tails, large
corpora, true cold cache, ranking and MVCC-certified aggregate acceleration remain
separate evidence requirements. Do not enable a new path by default or close the
issue merely because structural work counts improve.
