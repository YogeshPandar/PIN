# API evidence and local obligations

Verified against PostgreSQL 18.6 (`724edf9bde9d356724ad384a2e196edc3c9f80f7`),
pgrx 0.19.2 (`70383e884582d1bcc7cd681d10886b995a2830cb`), and Rust 1.98.1.
Review date: 18 September 2026. G0 boundary run 62 passed on code head
`6b4c93938f6fd252ed66ebf0debafcc352769eed`; Review artifacts run 26 passed
on the same head. This records observed CI evidence, not an independent unsafe
approval.

Every unsafe entry below still needs an independently assigned reviewer. No
reviewer is recorded as having approved it. G0 executable validation is green;
independent unsafe review remains outstanding.

## ABI01: independent header and binding probes

Modules: `crates/pin-pg/cshim/`, `crates/pin-pg/src/abi.rs`.

Authority: [AM layout](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/access/amapi.h),
[heap capacity](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/access/htup_details.h),
[item pointers](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/storage/itemptr.h),
[generic WAL capacity](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/access/generic_xlog.h),
[page layout](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/storage/bufpage.h), and Rust
[`offset_of!`](https://doc.rust-lang.org/1.98.1/std/mem/macro.offset_of.html),
[`size_of`](https://doc.rust-lang.org/1.98.1/std/mem/fn.size_of.html),
[`align_of`](https://doc.rust-lang.org/1.98.1/std/mem/fn.align_of.html).

The C probe accepts all `u32` keys, returns scalar `u64` values, and returns
`UINT64_MAX` for unknown keys. It cannot allocate, throw, dereference caller
memory, or access PostgreSQL state. Its ordinary `extern "C"` Rust declarations
are limited to these two non-throwing functions. Separate C and Rust field lists
compare every one of the 51 AM offsets. The Rust initializer is exhaustive.
C static assertions and runtime comparison check the selected page size,
alignment, pointer/Datum domains, heap offset capacity, and WAL registration
limit. No guessed four-page generic-WAL limit is used. No on-disk bytes are
encoded by these probes. Compiled layout agreement is not a server-fork audit.

Evidence: compiler static assertions; `pin.abi_check()`; `smoke.sql`; source
inventory tests and deliberately mutated inventories. G0 boundary run 62
compiled the C/Rust boundary and passed the runtime comparison against the
pinned PostgreSQL 18.6 build. Python inventory checks remain drift detection,
not a replacement for compiled validation.

## AM01: registration, allocation, and guarded callbacks

Module: `crates/pin-pg/src/am.rs`.

Authority: [AM API](https://www.postgresql.org/docs/18/index-api.html),
[callback contracts](https://www.postgresql.org/docs/18/index-functions.html),
[exact signatures](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/access/amapi.h),
[zero-argument handler dispatch](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/index/amapi.c),
[allocation declarations](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/utils/palloc.h),
pgrx [Internal](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx/src/datum/internal.rs),
[guard expansion](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx-macros/src/rewriter.rs), and Rust
[`ptr::write`](https://doc.rust-lang.org/1.98.1/std/ptr/fn.write.html).

Construct a complete Rust value first, then allocate exactly one PostgreSQL
memory-context chunk. `palloc` supplies the checked alignment and either returns
storage or raises ERROR through pgrx. `write` initializes it without reading the
uninitialized destination. The pointer is converted to a Datum without adopting
it into Rust allocator ownership. The caller's PostgreSQL context owns the
returned node; no Rust destructor frees it. No buffers, locks, registrations, or
persistent pages are acquired.

The SQL handler declares a dummy `internal` argument and returns
`index_am_handler`; it must not be STRICT. PostgreSQL calls it through
`OidFunctionCall0` with **zero actual arguments**. Its Rust signature therefore
has no parameter, and explicit SQL preserves the required catalog signature.
Reading the SQL dummy as a Rust argument would violate this dispatch contract. The handler requires an initialized UTF-8 backend. Its preload
path has already validated ABI and server presets. Every installed callback uses
`#[pg_guard]` with `extern "C-unwind"`, matching the selected bindings.

G0 installs no opclass and no tuple/bitmap scan entry. All capability booleans are
false. Storage callbacks fail with SQLSTATE `0A000` before dereferencing inputs.
`amvalidate` returns false; `amadjustmembers` rejects unreviewed opfamilies and
opclasses. `amoptions` rejects validation requests but returns null during
nonvalidating catalog loads. The end callback has nothing to release because no
begin callback can return a scan. This is a registration spike, not an index.

Evidence: SQL catalog/capability tests, rejected CREATE INDEX and CREATE OPERATOR
CLASS, and test-build destructor accounting around actual `amadjustmembers`
errors. No allocation or throughput benchmark is claimed. The implementation
avoids zeroing the whole AM node before overwriting it; this is not a measured
end-to-end performance result.

## COMPAT01: preload, server presets, and database encoding

Modules: `src/lib.rs`, `src/compatibility.rs` within `pin-pg`.

Authority: [loading rules](https://www.postgresql.org/docs/18/xfunc-c.html),
[preload settings](https://www.postgresql.org/docs/18/runtime-config-client.html),
[preset options](https://www.postgresql.org/docs/18/runtime-config-preset.html),
[startup order](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/postmaster/postmaster.c),
[GUC string lifetime](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/utils/misc/guc.c),
[dynamic loading](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/utils/fmgr/dfmgr.c),
[encoding API](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/mb/pg_wchar.h), and Rust
[`CStr::from_ptr`](https://doc.rust-lang.org/1.98.1/std/ffi/struct.CStr.html#method.from_ptr).

`_PG_init` reads only initialized server presets and the host preload flag.
Postmaster configuration precedes shared-library preloading. G0 checks exact
`server_version_num = 180006` and `block_size = 8192`; a minor-version port
requires changing and revalidating that policy. `GetConfigOption` receives fixed
nul-terminated names. Its borrowed, terminated string is compared immediately
without another host call or a retained pointer, and is never modified or freed.
No database-encoding lookup occurs during postmaster preload. Backend entry
points check UTF-8 independently, including installation into a LATIN1 database.

Preloading is required, but G0 does not allocate shared coordination state yet.
No mutable "initialized" fallback flag can turn a failed late load into support.
PostgreSQL does not unload a shared library on DROP EXTENSION: the tests cover
catalog removal/recreation and server restart, not library unloading.

Evidence: normal/restarted cluster diagnostics, repeated same-backend and
fresh-backend late LOAD rejection, failed-install catalog rollback, and LATIN1
rejection. A real incompatible-server-build negative test remains unrun.

## ERR01: PostgreSQL ERROR, Rust panic, and test-only cleanup probes

Module: `crates/pin-pg/src/test_hooks.rs`; guarded callback in `src/am.rs`.

Authority: pgrx [guard implementation](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx-macros/src/rewriter.rs),
[panic translation](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx-pg-sys/src/submodules/panic.rs),
[SPI](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx/src/spi.rs),
[Rust FFI unwinding](https://doc.rust-lang.org/nomicon/ffi.html),
[`Cell`](https://doc.rust-lang.org/1.98.1/std/cell/struct.Cell.html), and
[`LocalKey::try_with`](https://doc.rust-lang.org/1.98.1/std/thread/struct.LocalKey.html#method.try_with).

The release and development profiles retain `panic = "unwind"`. Fixed error
probes compile only with the existing `test-hooks` feature. Default builds omit
the functions. Test builds revoke PUBLIC execution privileges; SQL tests verify
these ACLs for a separate unprivileged role. No caller supplies a pointer, SQL
fragment, or crash instruction to a probe.

A stack-owned probe counts Drop using a thread-local Cell. Drop uses `try_with`
and saturating arithmetic, makes no PostgreSQL call, and never allocates or
panics. This instrumentation is not a production memory-budget implementation.
The SPI probe raises fixed SQLSTATE/message/detail values inside PostgreSQL.
The AM probe is called by PostgreSQL while defining an unsupported opclass,
exercising the real registered C-to-Rust callback rather than a direct helper.

Evidence: `errors.sql` repeats four failures in 32 rounds and requires exactly
128 destructor increments, preserved SQLSTATE and PostgreSQL detail, no surviving
opclass, and continued backend operation. G0 boundary run 62 passed this guarded
error-probe suite with the `test-hooks` build after host-boundary Clippy passed
with warnings denied. These tests do not prove buffer/ResourceOwner cleanup or
backend-death recovery, because G0 has not acquired those resources.

## BUILD01: native C compilation and matching host headers

Module: `crates/pin-pg/build.rs`.

Authority: [Cargo build scripts](https://doc.rust-lang.org/cargo/reference/build-scripts.html),
[`Command`](https://doc.rust-lang.org/1.98.1/std/process/struct.Command.html), and
[`pg_config`](https://www.postgresql.org/docs/18/app-pgconfig.html).

The script requires the absolute `PGRX_PG_CONFIG_PATH` used by pgrx, native
x86_64 Linux GNU, and PostgreSQL 18.6 headers. The Linux C probe defines
`_GNU_SOURCE`, matching PostgreSQL 18.6's Linux build template, while retaining
strict C11 warnings as errors. Compiler and archiver arguments are passed as
separate arguments, not shell commands. `CC` and `AR`, when set, must be
executable paths, not shell command strings. It checks every child exit code and
UTF-8 metadata result, writes only to OUT_DIR, and never downloads tools. Cargo
watches the selected pg_config, header tree, and local C inputs. The existing
dependency graph and Cargo-generated lockfile remain unchanged.

Evidence: G0 boundary run 62 passed manifest/source policy checks, Rust
formatting, pure tests, pure Clippy, rustdoc, pinned PostgreSQL 18.6 compilation,
C probe compilation, extension linking/install, disposable-cluster lifecycle
tests, host-boundary Clippy, test-hook installation, and guarded error/permission
tests. No native Rust or PostgreSQL tool is installed in the development VM.

## ID01 / RS01: unchanged pure identity and accounting contracts

Modules: `pin-core/src/identity.rs`, `pin-core/src/budget.rs`.

Authority: [item pointer domains](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/storage/itemptr.h),
[heap capacity](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/access/htup_details.h),
[`NonZero`](https://doc.rust-lang.org/1.98.1/std/num/struct.NonZero.html), and
[`usize::checked_add`](https://doc.rust-lang.org/1.98.1/std/primitive.usize.html#method.checked_add).

These existing APIs are not changed by the host-boundary follow-up. Coordinates
validate domains, not MVCC visibility. Accounting validates arithmetic, not
allocator interception or automatic Drop cleanup. The existing core crate
forbids unsafe code; no PostgreSQL pointer enters it.

## MODEL01: independent publication/reclamation model

See [publication-model.md](publication-model.md) for exact state/edge coverage,
independent enumeration, negative controls, and limits. Rust reference APIs are
[`HashMap::entry`](https://doc.rust-lang.org/1.98.1/std/collections/struct.HashMap.html#method.entry),
[`VecDeque`](https://doc.rust-lang.org/1.98.1/std/collections/struct.VecDeque.html), and
[`Option::is_none_or`](https://doc.rust-lang.org/1.98.1/std/option/enum.Option.html#method.is_none_or).
The fixture does not certify an implemented PostgreSQL WAL or pin protocol.

## G4QUERY01: streaming bitmap query execution

Modules: `pin-core/src/mutable/{query,page,reader}.rs` and `pin-pg/src/am.rs`.
The preceding G0 entries are historical; current storage contracts are in
[g2-storage.md](g2-storage.md) and [g3-storage.md](g3-storage.md).

Authority: PostgreSQL 18 [scanning](https://www.postgresql.org/docs/18/index-scanning.html)
and [locking](https://www.postgresql.org/docs/18/index-locking.html), pinned
[`index_getbitmap`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/index/indexam.c#L757-L783),
Rust 1.98.1 [`Vec::try_reserve_exact`](https://doc.rust-lang.org/1.98.1/std/vec/struct.Vec.html#method.try_reserve_exact),
[`slice::get`](https://doc.rust-lang.org/1.98.1/std/primitive.slice.html#method.get),
[`slice::split_at_mut`](https://doc.rust-lang.org/1.98.1/std/primitive.slice.html#method.split_at_mut), and
[conditional chains](https://doc.rust-lang.org/reference/expressions/if-expr.html#let-chain).
Rechecked on 18 September 2026, including the immutable bitmap dispatch source.

The executor intersects/unions stable owner identities, never approximate
negation or reusable heap coordinates. Every emitted candidate retains the
existing `BitmapSink` heap recheck. PostgreSQL keeps MVCC authority; the returned
count is statistical, not a visible result count. The same G3 shared barrier
covers private posting-page copies through the last candidate. No new unsafe
operation, PostgreSQL pointer retention, disk format or WAL change is introduced.

Private decoder offsets replace self-referential borrows. Actual vector capacities
bound cursor pages and continuation scratch. A cursor-budget failure falls back
only before I/O/emission; host errors propagate without replay. Separate active
term occurrences retain independent cursor positions while immutable dictionary
metadata is resolved once per unique active term. Direct one- and two-term roots
avoid continuation-stack interpretation; disjoint cursor mutation uses checked
indices plus `split_at_mut`. A stale owner copy refreshes once when an appended
slot exceeds its count, without hiding genuine corruption.

Evidence: the original 13 G4 Rust regression tests passed with all 80 non-ignored core tests,
core Clippy and rustdoc at `6020d5bc50288c6dc029d76f974284a22db83756` in G0
boundary run 179. The job also exposed the corrected host closure formatting.
SQL equality and restart checks are wired through `tools/g2_qualification.sh`.
See [g4-query-execution.md](g4-query-execution.md) for examples, precise bounds,
work-count evidence, observed versus pending qualification, and fallback limits.

Review: implementation self-review completed; final-head CI is recorded in PR #8.
No independent reviewer or performance qualification is claimed. Existing unsafe
host-boundary review remains outstanding, and bare-PostgreSQL parity is unmeasured.


## G5COUNT01: owner-pinned direct counts and VM certification

Modules: `pin-core/src/mutable/{count,vacuum}.rs`,
`pin-pg/src/{count,storage,native}.rs`, and
`pin-pg/cshim/{pin_count,pin_storage}.c`.

Authority: PostgreSQL 18.6 pinned
[visibility-map implementation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/visibilitymap.c),
[index-only VM ordering](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/executor/nodeIndexonlyscan.c),
[buffer cleanup-pin contract](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/storage/buffer/README),
[buffer API](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/storage/bufmgr.h),
[table index fetch](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/access/tableam.h),
[CustomScan execution](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/executor/nodeCustom.c),
[CustomPath plan creation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/optimizer/plan/createplan.c),
[setrefs CustomScan handling](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/optimizer/plan/setrefs.c), and
[path lifetime](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/optimizer/util/pathnode.c).
Rust 1.98.1 contracts used in the guarded adapter are
[`slice::from_raw_parts`](https://doc.rust-lang.org/1.98.1/std/slice/fn.from_raw_parts.html),
[`str::from_utf8`](https://doc.rust-lang.org/1.98.1/std/str/fn.from_utf8.html), and
[checked integer arithmetic](https://doc.rust-lang.org/1.98.1/std/primitive.u64.html).

`visibilitymap_get_status` does not lock the VM page and explicitly leaves
concurrency to the caller. Pin rereads status for each eligible candidate and
retains a canonical owner buffer pin from the fresh publication/liveness copy
through either VM certification or `table_index_fetch_tuple`. The content lock
used for the private owner copy is released before VM, heap work or a test pause.
No all-visible boolean is cached in Rust.

VACUUM owner removal is not an ordinary PageStore commit. The PostgreSQL adapter
uses `LockBufferForCleanup` before the generic-WAL registration. The buffer
manager waits until the remover is the sole pin holder, so a count relying on its
copied owner state finishes before removal and possible heap-slot reuse. Unknown
PageStore adapters fail closed. The existing writer interlock protects the private
read/modify/write image while cleanup permission protects count readers.

Heap fallback uses `table_index_fetch_tuple` with a private root copy and the
executor MVCC snapshot, preserving HOT semantics. The original posting/owner
identity remains unchanged. Mutable or otherwise uncertified candidates never
use the VM shortcut. SERIALIZABLE and recovery-time custom execution are
excluded. Both count GUCs are `PGC_SUSET` and default off.

The upper path first decodes the already-validated constant query through the
same bounded Rust query decoder and is offered only for an exact single-term
root. Compound queries remain on the core aggregate path.

The upper path retains an actual core aggregate child for execution-time
fallback. Because `add_path` can immediately free a dominated non-IndexPath,
Pin shallow-copies the AggPath before insertion. Candidate costing retains the
eligible core aggregate's full heap/index estimate and credits only the omitted
aggregate transition. The plan adds the private index OID to relation
dependencies. Execution revalidates relation/index identity, attribute/type,
collation, operator family, validity/readiness/liveness and `indcheckxmin`
before reading storage.

Evidence authored in this change: `g5_count.rs`, the source-contract tests,
the exhaustive bounded visibility model and its negative controls,
`tests/sql/g5_counts.sql`, and `tools/g5_qualification.sh`. Local Python/model
checks are independent of Rust compilation. Rust/C compilation, PostgreSQL
integration schedules, crash recovery and independent visibility review remain
unobserved until CI/host execution. See [g5-counts.md](g5-counts.md).


## G7BUILD01: PostgreSQL parallel build lifecycle

Modules: `crates/pin-pg/src/am.rs`, `crates/pin-pg/src/native.rs`,
`crates/pin-pg/cshim/pin_parallel.c`.

Authority: PostgreSQL 18.6 commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7`,
`src/backend/catalog/index.c`, `src/include/access/tableam.h`,
`src/include/access/parallel.h`, `src/backend/access/transam/parallel.c`,
and `src/backend/access/gin/gininsert.c`.

PostgreSQL supplies `IndexInfo.ii_ParallelWorkers`. Pin enters parallel mode,
creates a `ParallelContext`, initializes one parallel table scan, and launches
no more participants than fit the fixed per-participant preparation ceiling
inside `maintenance_work_mem`. Shared state contains relation OIDs, scalar
statistics, the table-scan descriptor and one fixed memory limit. Workers reopen
relations with the nonconcurrent build lock modes and call
`table_index_build_scan` with `BuildIndexInfo`.

The Rust worker callback receives only the live callback arguments and the
validated scalar memory limit. It analyzes the document before acquiring Pin's
existing writer interlock, then uses the same generic-WAL publication path as
serial build. The worker does not retain `Datum`, `ItemPointer`, relation or
value pointers after the callback.

Fallback: concurrent index build, zero requested workers, unavailable DSM or zero
launched workers use the established serial build.

Validation: C/Rust compilation, AM capability inspection, deterministic stage-16
worker observation, serial/indexed equality and PostgreSQL transactional/recovery
qualification. No build-speed claim is attached to this boundary.

## G7COUNT01: disjoint DSM direct-count execution

Modules: `crates/pin-core/src/mutable/work.rs`,
`crates/pin-pg/src/count.rs`, `crates/pin-pg/cshim/pin_count.c`.

Authority: PostgreSQL 18.6 `src/include/access/parallel.h`,
`src/backend/access/transam/parallel.c`, `src/backend/executor/nodeCustom.c`,
the CustomScan execution manual, and the existing G5 visibility authorities.
Rust authority: Rust 1.98.1
`slice::from_raw_parts` and checked integer conversion documentation.

DSM contains only relation OIDs, an attribute number, immutable query bytes,
eleven validated `u64` work words, scalar counters and a PostgreSQL spinlock.
The spinlock protects fixed copies and compare-and-replace only. Page reads,
query decoding, Rust execution, visibility checks and heap fetches occur after
the spinlock is released.

Rust constructs the query slice only after C supplies a non-null pointer and a
length bounded by `isize::MAX`. The borrow lasts only for the synchronous
decode call. No shared-memory bytes become a mutable Rust reference. Work batches
are private page copies. A successful claim assigns one batch to one participant;
a failed claim discards the private copy.

Workers use PostgreSQL-restored active MVCC snapshots and reopen their own
relations, fetch state, slot and scratch context. Candidate visibility and count
certification remain the G5 protocol. A worker failure aborts the whole SQL
statement rather than returning or retrying partial accounting.

Validation: pure competing-claim tests, malformed shared-state tests,
deterministic stage-17 worker observation, worker termination, exact serial count
equality and successful execution after failure.

## G7VACUUM01: PostgreSQL-managed parallel VACUUM

Modules: `crates/pin-pg/src/parallel.rs`,
`crates/pin-pg/cshim/pin_parallel.c`.

Authority: PostgreSQL 18.6 `src/include/access/amapi.h`,
`src/include/access/genam.h`, `src/include/commands/vacuum.h`, and
`src/backend/commands/vacuumparallel.c`.

The restart-only capability gate defaults off. When enabled, Pin advertises only
parallel bulk-delete and cleanup. PostgreSQL owns worker scheduling and copies the
standard `IndexBulkDeleteResult` across DSM. Pin appends no private state.

`IndexVacuumInfo.strategy` remains callback-owned and is borrowed only for the
synchronous phase. Page reads use `ReadBufferExtended` with that strategy.
`vacuum_delay_point(false)` runs at traversal boundaries outside buffer-content
locks and generic-WAL batches.

Validation: exact stats representation, serial fallback, cancellation and
worker-failure qualification, plus existing VACUUM/recovery suites.

## G7MERGE01: retained sealed-prefix ownership transfer

Modules: `crates/pin-core/src/mutable/compact.rs`,
`crates/pin-pg/src/maintenance.rs`.

This is not reference-counted cross-generation sharing. Under the existing
exclusive structural barrier and writer interlock, a completely live sealed
prefix remains owned by the same active term chain while its suffix is replaced.
Publication updates metapage, dictionary and boundary together in the existing
bounded generic-WAL batch. Only the detached suffix enters the retirement
journal.

The copying compactor remains the default differential reference. The opt-in
control is privileged and default-off. Pure and host recovery tests must prove
that retained pages never enter the free list while reachable.
