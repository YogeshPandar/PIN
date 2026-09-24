# G9 PostgreSQL integration

Status: default-off implementation, locally qualified on PostgreSQL 18.6 at
merged commit `86d49b984df277c14f453698a118a68920b6604b`. The physical core
is connected to PostgreSQL maintenance and bitmap scans. Native normal and
test-hook qualification passed locally on 24 September 2026. See the
[G9 run record](runs/2026-09-24-g9-review/README.md) for measured performance,
index size and limitations. Production readiness remains unestablished.

The supported host remains PostgreSQL 18.6, pgrx 0.19.2, Rust 1.98.1 and native
x86_64 Linux GNU. The scalar `pin-kernels` library supports `no_std` without default
features. The PostgreSQL host is not freestanding: it deliberately uses the server's
buffers, WAL, memory contexts, temporary-file ownership and bitmap implementation.

## Implementation map

| Component | Responsibility |
| --- | --- |
| `pin-core/src/grouped/` | Checked logical records, immutable membership, scalar Boolean evaluation and clear-only liveness |
| `mutable/page_grouped.rs` | Versioned metadata tail, physical fragments and catalog node framing |
| `mutable/grouped/build.rs` | Complete-owner capture, bounded sort stream, sealing and publication |
| `mutable/grouped/storage.rs` | Catalog search, record loading, journal ownership, allocation and recovery |
| `mutable/grouped/vacuum.rs` | Shared-liveness retirement before canonical owner removal |
| `mutable/grouped/scan.rs` | Group-first Boolean execution plus a conservative mutable-write cover |
| `pin-pg/src/grouped.rs` | Default-off settings and guarded, bounded native-sort calls |
| `pin-pg/cshim/pin_grouped.c` | Batched PostgreSQL tuplesort, borrowed-datum copying and explicit normal cleanup |
| `pin-pg/src/am.rs` | Build, VACUUM cleanup and `amgetbitmap` integration using existing barriers and bitmap sink |

The C build now carries PostgreSQL's `-fno-strict-aliasing`, `-fwrapv` and
`-fexcess-precision=standard` semantics. These are correctness constraints for
server-header code, not benchmark-driven optimization switches.

No dependency or lockfile change is required. The existing generic-WAL adapter is
reused; a custom resource manager, private page cache and private spill-file system
are not introduced.

## Activation and fallback

Both settings are superuser-controlled and default to `off`:

| Setting | Effect |
| --- | --- |
| `pin.enable_grouped_storage` | Allows supplemental snapshot creation after CREATE INDEX and during VACUUM cleanup |
| `pin.enable_grouped_scan` | Allows supported Boolean bitmap scans to use an existing grouped snapshot |

Use only a disposable qualification database until independent review and the
remaining operational gates pass. For example, after installing and preloading
the candidate binary:

```sql
SET maintenance_work_mem = '64MB';
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) public.documents;

SET pin.enable_grouped_scan = on;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SET enable_seqscan = off;
EXPLAIN (ANALYZE, BUFFERS)
SELECT id FROM public.documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha AND NOT beta');
```

The planner settings above select the bitmap path for diagnosis, not general
production tuning. CREATE INDEX also builds a snapshot when the storage setting
is enabled. Existing heap-build participants finish before grouped publication.
A build that cannot reserve its scratch and minimum sort budget skips grouped
publication, preserving the canonical index and any existing active snapshot.

Phrase and prefix queries, unsupported program sizes, insufficient query scratch
and indexes without an active snapshot fall back before emitting grouped output.
Plain index scans, parallel plain scans, count experiments and positional execution
continue through their existing paths. `BitmapSink` retains PostgreSQL's existing
bitmap, batches TIDs and preserves recheck flags, including multiple SQL scan keys
and lossy bitmap behavior. No grouped result establishes heap snapshot visibility.

## Identity, publication and recovery

A maintenance operation acquires the exclusive structural barrier before the
writer interlock. It captures complete published canonical owners and term coverage.
A segment cannot contain two live incarnations at one heap root. The membership
record fixes each coordinate's owner incarnation; term masks never intersect across
independent segment identities. Logical encoding alone is not a provenance proof.

The physical adapter is scoped to one live index relation. Its logical relation
namespace is currently `1`, not a globally unique database identifier. Segment IDs
come from that relation's durable, nonwrapping incarnation allocator. Record loaders
check the expected segment, kind, term/group key, layout and catalog mask. Never
combine records from different relations or carry them across REINDEX by comparing
only their numeric logical keys.

A snapshot stores its immutable owner cutoff, catalog root and page-chain ownership.
The optional `PG09` version-1 metadata tail leaves legacy metadata readable by the
new binary. Fragments and catalog nodes are ordinary index pages. Catalog leaves
are ordered by term identity and 256-page base; parent nodes bound catalog seeks.

An unreachable build journal owns output before publication. One metadata update
publishes the complete snapshot and transfers the prior chain to retirement
ownership. Journal recovery can discard unpublished output or finish retirement;
it never makes partially sealed term coverage searchable. Each commit respects the
existing adapter's three-page bound. Generic WAL modifies registered private images
under exclusive buffer locks and applies the standard page boundaries. Publication
requires reader quiescence before old catalog or fragment pages are reclaimed.

The write path remains canonical and immediately searchable. After grouped Boolean
evaluation, the current adapter emits every newer live published owner as a
**recheck-required candidate**, starting after the captured owner cutoff. It does
not yet evaluate the query selectively within that delta. PostgreSQL applies the
full predicate and MVCC visibility, so a large delta may be expensive even when the
sealed snapshot prunes well. It is not an exact candidate count or a no-recheck
mutable index. Skipping a rebuild requires checking the exact saved owner anchor;
a changed owner cutoff triggers a new snapshot, while deletion-only maintenance
can retain the cleared snapshot.

## VACUUM and locking

Shared liveness is retired before canonical owner removal and before the access
method returns permission for heap slot reuse. Retirement remains unconditional
when either or both experimental settings are disabled. Every relevant published
copy, including legacy direct-posting copies, must be retired. Failure aborts the
maintenance operation; it must not be converted into successful heap reuse.

Liveness changes clear bits only, preserve payload widths and directory positions,
and reject restoration of bits from a stale image. The writer interlock excludes
competing retirement/publication writers. Grouped readers retain the existing
structural shared barrier; they copy pages under the existing buffer protocol and
retain no borrowed shared-buffer pointer after unlock.

No `XLogFlush` is added per bit or per group. WAL insertion and the subsequent heap
cleanup obey PostgreSQL's WAL-before-data and index-before-heap reuse contracts.
The crash test's synchronous witness transaction is a testing device for forcing
prior WAL durability, not a proposed per-delete flush policy.

The snapshot builder is currently a full rebuild while holding both maintenance
barriers. It is not a background or concurrent segmented merge. Long builds can
block writes and structural readers. Normal maintenance uses the callback's buffer
strategy and existing cost-delay path. The AM now advertises maintenance-memory use
so PostgreSQL can budget parallel VACUUM participants; grouped parallel worker
execution still needs native observation and qualification.

## Memory and native sort contract

`build_memory(layout)` reserves the largest supported membership byte image, one
heap group's owner and offset vectors, catalog levels and fixed working margin.
Its size depends on the checked offset domain, not the relation's row count. This
is a logical buffer reservation, not a measured upper bound on process RSS or
allocator/context overhead.

The C bridge rounds that reservation up to KiB and subtracts it from
`maintenance_work_mem`, or from `autovacuum_work_mem` in an autovacuum worker when
that override is set. At least 64 KiB remains for tuplesort; otherwise it returns
without starting a sort or publishing new grouped state. PostgreSQL's tuplesort
owns spilling under the residual budget. Concurrent workers' budgets and total
backend RSS must still be measured.

A sort record is an alignment-one, 32-byte big-endian key. The bridge submits at
most 256 records per guarded call. Each input is wrapped in a stack-local four-byte
varlena header and copied by PostgreSQL before reuse. Forward output uses
`tuplesort_getdatum(..., copy=false, ...)`; C validates and copies 32 bytes into the
Rust caller's output before any subsequent sort operation invalidates that datum.
No per-record output allocation or PostgreSQL pointer enters the pure core.

The handle has one insertion-to-reading transition and an explicit consuming close.
Ordinary Rust `Result` failures close the sort before propagating. PostgreSQL ERROR
and cancellation rely on its context and ResourceOwner cleanup through the existing
pgrx guarded boundary. No destructor calls back into PostgreSQL. The five new FFI
operations and their remaining review gates are registered as `G9PG01`.

## Qualification commands and evidence

```bash
python3 -m unittest discover -s tests -v
python3 tools/check_contracts.py
cargo fmt --all --check
cargo check --locked -p pin-kernels --no-default-features --lib
cargo test --locked -p pin-core --test g9_storage
cargo test --locked --release -p pin-core --test g9_storage
cargo clippy --locked -p pin-core -p pin-kernels --all-targets -- -D warnings
cargo clippy --locked -p pin-pg --features test-hooks --all-targets -- -D warnings
```

In a native environment, install the normal extension, then run:

```bash
export PGRX_PG_CONFIG_PATH=/path/to/postgresql-18.6/bin/pg_config
bash tools/g9_qualification.sh
```

After installing the `test-hooks` build, run:

```bash
PIN_G9_TEST_HOOKS=1 bash tools/g9_qualification.sh
```

The shell script creates and destroys only its disposable cluster. The driver
checks full row identities and bitmap plans, not just counts. It covers actual
TID reuse, HOT-related updates, mixed legacy/grouped reads, immediate and aborted
writes, low-memory fallback, observed spill, canceled spill-file cleanup, lossy
bitmaps, NOT/difference, permissions, REINDEX and disabled-gate retirement. Test
hooks prove grouped-path selection and pause reservation, storage, publication,
reclamation, retirement and sort completion. Hard-crash cases commit a heap-only
WAL witness before immediate shutdown, then check results before and after recovery.
Reader/writer/maintenance schedules and a repeatable-read snapshot test are included.

Local evidence for the adapter work: **80 Python test methods passed**, including
compiling the actual C bridge against isolated test doubles at `-O0` and at `-O2`
with undefined-behavior/bounds sanitizers. That marshalling harness checks 300
records, batch boundaries, invalid calls, borrowed-buffer invalidation, unaligned
output, memory preflight and normal cleanup. It does not link PostgreSQL, execute
Rust, model real sorting/spill, or certify the PG ABI, locks, WAL or error cleanup.

Four additional physical-core Rust tests cover cutoff detection, scratch preflight,
sort-ready failure and positive grouped-scan selection. The release Rust suite,
normal and test-hook native qualification, 80 Python tests, source-contract checks
and formatting check passed locally on 24 September 2026. Independent
unsafe/storage review remains open.

## Migration limits and rollback

Canonical owners, legacy postings and positional payloads remain stored. This is a
supplemental migration path, not completed legacy-format removal or smaller indexes.
Stop using the experimental executor immediately with:

```sql
SET pin.enable_grouped_scan = off;
SET pin.enable_grouped_storage = off;
```

Disabling settings does not remove grouped metadata or make an old binary capable
of VACUUMing those pages. Before any downgrade, use the current binary with grouped
storage disabled to REINDEX every affected index, then verify the legacy-only
representation and follow the existing extension downgrade procedure. REINDEX is
not transactional with a later binary replacement; plan that operational change
separately. The safe default is to keep the new binary while switching both gates
off, not to load an older reader over new pages.

Next performance work includes selective bounded delta evaluation, incremental
segmented compaction, catalog/fragment I/O measurements, memory accounting and a
matched GIN benchmark including write latency, WAL, index size and recovery costs.
SIMD, exact counts and ranked top-k remain separate gates. A similar bitmap shape
is not evidence of TIN-equivalent performance.

## Official contracts

Pinned references and exact obligations are in [api-evidence.md](api-evidence.md),
entry `G9PG01`. See also [the logical format](g9-grouped-storage.md) and
[the unsafe boundary register](unsafe-audit.md).
