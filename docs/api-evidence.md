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


## G7SCAN01: native plain and parallel index scans

Modules: `crates/pin-core/src/mutable/work.rs`,
`crates/pin-pg/src/am.rs`, `crates/pin-pg/src/storage.rs`,
`crates/pin-pg/cshim/pin_storage.c`.

Authority: PostgreSQL 18.6 commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7`,
`src/backend/access/index/indexam.c`,
`src/backend/executor/nodeIndexscan.c`,
`src/backend/optimizer/path/indxpath.c`,
`src/backend/optimizer/path/costsize.c`, and the PostgreSQL 18 Index AM
parallel-scan contract.

`amcanparallel` is enabled only with `amgettuple`,
`amestimateparallelscan`, `aminitparallelscan`, and
`amparallelrescan` present. PostgreSQL excludes bitmap index paths from this
AM parallel interface, so bitmap construction remains serial while plain
Index Scan participants use the new shared work protocol.

AM DSM contains only a PostgreSQL spinlock, a ready flag and eleven checked
`u64` work words. Each backend owns one fixed root batch. Work preparation
copies one page outside the spinlock; compare-and-replace assigns that batch to
exactly one participant. Canonical owner liveness is reread before a root is
returned. The AM sets `xs_recheck` for every TID, leaving MVCC and exact
operator semantics to PostgreSQL.

A shared structural barrier is acquired by each plain scan at `amrescan` and
held through `amendscan`; this prevents posting-page reclamation while captured
work is consumed. No Rust allocation, page image, relation pointer, snapshot
pointer or buffer handle enters DSM. Rescan clears the shared scalar state and
the next participant captures fresh work.

Validation: source-policy tests, full-row serial/parallel equality, actual
parallel Index Scan workers, leader-on/off execution, post-claim worker
termination, subsequent reuse, host Clippy and PostgreSQL recovery qualification.

## G7BUILD01: PostgreSQL parallel build lifecycle

Modules: `crates/pin-pg/src/am.rs`, `crates/pin-pg/src/native.rs`,
`crates/pin-pg/cshim/pin_parallel.c`.

Authority: PostgreSQL 18.6 commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7`,
`src/backend/catalog/index.c`, `src/include/access/tableam.h`,
`src/include/access/parallel.h`, `src/backend/access/transam/parallel.c`,
`src/backend/access/gin/gininsert.c`, `src/backend/access/nbtree/nbtsort.c`,
`src/backend/storage/lmgr/lock.c`, `src/backend/storage/lmgr/README`, and
`src/include/storage/lwlock.h`.

PostgreSQL supplies `IndexInfo.ii_ParallelWorkers`. Pin enters parallel mode,
creates a `ParallelContext`, initializes one parallel table scan, and launches
no more participants than fit a conservative preparation, detoast and fixed-state
peak inside one `maintenance_work_mem` budget. Shared state contains relation
OIDs, scalar statistics, the table-scan descriptor and the preparation limit.
Workers reopen
relations with the nonconcurrent build lock modes and call
`table_index_build_scan` with `BuildIndexInfo`.

The Rust worker callback receives only the live callback arguments, validated
scalar memory limit and an opaque pointer to a build-DSM `LWLock`. PostgreSQL
lock-group members do not conflict on ordinary heavyweight locks, so the
existing Pin page-lock interlock alone cannot serialize parallel build
participants. Analysis stays outside the DSM lock. Rust asks the C boundary to
take the DSM `LWLock` only around the existing writer interlock and generic-WAL
publication, then releases it before returning. PostgreSQL error cleanup releases
held LWLocks on failure. Rust never dereferences the shared lock.

Parallel scans use participant-local `IndexInfo` values. Each participant records
`ii_BrokenHotChain` in the shared build state, and the leader propagates the
OR result to the original core-owned `IndexInfo` before returning from
`ambuild`. Core can therefore preserve the normal nonconcurrent HOT safety
horizon.

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

Worker admission is also memory bounded. The immutable query/control DSM chunk
is charged against one `work_mem` budget first. The remainder must fit leader
plus requested workers using a conservative peak that includes two query budgets,
analyzer memory, maximum detoasted input and fixed batch state.

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
Traversal uses interrupt-only checks under Pin interlocks. Cost-delay sleeps run
before and after the writer/structural critical section, with no Pin interlock,
buffer-content lock or generic-WAL batch held.

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

## SQL02: ordinary single-term predicate streaming (2026-09-22)

Module: `crates/pin-pg/src/matching.rs`; existing pure matcher in
`crates/pin-core/src/recheck.rs`. No new unsafe operations or dependencies.

Reviewed PostgreSQL 18.6 executor source at
`724edf9bde9d356724ad384a2e196edc3c9f80f7`,
`src/backend/executor/nodeBitmapHeapscan.c`, especially BitmapHeapNext and
BitmapHeapRecheck. Heap visibility and executor rechecks remain PostgreSQL's
responsibility. This patch changes the predicate's internal evaluation only;
it does not certify postings or suppress rechecks.

Reviewed pgrx 0.19.2 (`70383e884582d1bcc7cd681d10886b995a2830cb`),
`pgrx/src/datum/from.rs` implementations for `&str` and `&[u8]`, and
[pg_extern](https://docs.rs/pgrx/0.19.2/pgrx/attr.pg_extern.html).
Arguments remain borrowed only for the current call; no cached Datum, retained
PostgreSQL pointer, or new memory-context ownership is introduced.
[CREATE FUNCTION](https://www.postgresql.org/docs/18/sql-createfunction.html)
contracts for IMMUTABLE, STRICT and PARALLEL SAFE are unchanged: evaluation
uses only validated input and the fixed analyzer profile, with no table reads.

Reviewed Rust 1.98.1 (`48a229cea`) str::eq_ignore_ascii_case contract:
ASCII folding does not perform Unicode normalization. The existing matcher
uses it only for ASCII documents; other text uses the budgeted normalizer.
The single-term path validates the full input even after a match. Compound,
phrase and prefix expressions retain materialized oracle evaluation. Removing
scratch buffers can avoid allocation/budget failures of the old path; document,
term, token and work limits still apply. Cargo's documented `test --locked`
workflow preserves the committed dependency selection.

Validation obligations: pure matcher versus independent materialized oracle;
SQL sequential/index/custom-count result agreement; transaction, recovery and
parallel lifecycle qualification; measured ordinary SQL latency. The G6 SQL
suite compares streaming SQL predicates with count mode off (materialized
oracle) and on (streaming), preserving an independent reference.
Self-review only; existing independent FFI/storage review gates remain open.
See `docs/runs/2026-09-22-improvements/README.md` for observed results.

## AM02: predicate proof carried into PostgreSQL bitmaps (2026-09-22)

Authority: PostgreSQL 18 [index scanning](https://www.postgresql.org/docs/18/index-scanning.html),
[index locking](https://www.postgresql.org/docs/18/index-locking.html), and
[HOT](https://www.postgresql.org/docs/18/storage-hot.html). Inspected immutable
upstream `724edf9bde9d356724ad384a2e196edc3c9f80f7`:
`src/backend/nodes/tidbitmap.c` (tbm_add_tuples, union/intersection, private/shared
iteration), `src/backend/executor/nodeBitmapHeapscan.c` (BitmapHeapNext and
BitmapHeapRecheck), and `src/backend/utils/adt/tsginidx.c`
(gin_tsquery_consistent). GIN's exact versus maybe distinction is an upstream
example of this contract, not a proof for Pin's representation.

A false tbm_add_tuples recheck flag means exact satisfaction of all scan keys,
not snapshot visibility. Core still fetches heap tuples using the statement
snapshot and follows HOT chains. Core lossification and intersections with lossy
pages independently require predicate rechecks. Pin already rejects non-MVCC
snapshots in pin_scan_validate; the structural barrier covers index production.
No VM certification, heap bypass, lock lifetime, WAL or disk-format change.

The pure executor now emits `(root, requires_recheck)`. Only a successfully built
positive term/AND/OR plan can emit false. Matching initially uses canonical
OwnerRef, including incarnation. The experimental direct sealed path described in DT01 can read a copied live heap
coordinate after compaction; its VACUUM retirement and snapshot proof apply. Phrase,
prefix and negation shapes remain approximate. Every allocation-budget fallback
emits true, even if its input query was positive. This is critical: a term cover
for `a AND b` can contain rows with only `a`. Errors after emission still abort;
no partial-result fallback is added. Multiple SQL scan keys force true because
chosen_query evaluates only one of them. Plain index scans retain their existing
recheck behavior.

The existing C/Rust pin_bitmap_add ABI gains one bool, using the same mapped
PostgreSQL bool ABI already used throughout the shim. Existing pointer arrays,
bounds and synchronous copy lifetimes are unchanged. No new raw pointer access
or unsafe block is introduced. Read pgrx 0.19.2 (`70383e884582d1bcc7cd681d10886b995a2830cb`)
`src/guc.rs`: define_bool_guc retains static names and setting storage. The
static SUSET pin.enable_exact_bitmap control is captured once per bitmap sink;
off forces the original rechecks. It does not change SQL function semantics.
Rust's safe iterator/matches operations inspect the already validated AST and
retain no borrowed PostgreSQL data. Cargo.lock remains unchanged.

Tests: the exhaustive query matrix checks every false-recheck emission against
the independent document oracle. A 256-byte budget forces an AND term-cover
fallback and verifies true flags on its false-positive candidates. Host tests
check exact row identities and actual pin.matches calls through PostgreSQL's
continuously updated pg_stat_xact_user_functions; low work_mem must report real
lossy pages. Multi-key contradictions, positional false positives, Unicode,
HOT/indexed updates, own writes, rollback and VACUUM/reuse are included. Existing
multi-backend, restart/crash, parallel bitmap and RLS suites also apply.

Self-review status: proof and tests are recorded, not independent approval of
the extension's existing storage/FFI implementation. Release review remains open.

## R03: measured decoder inline hints (2026-09-22)

Read Rust's codegen inline attribute contract at
https://doc.rust-lang.org/reference/attributes/codegen.html#the-inline-attribute
for the existing Rust 1.98.1 toolchain. `#[inline]` is an optimization hint;
checked decoding, errors and iterator state remain unchanged. Four hints target
software-profiled varint and posting iterator boundaries. Full pure tests pass;
SQL identity checks and repeated measurements are archived under
`docs/runs/2026-09-22-exact-bitmap`. No FFI, storage or visibility contract changes.
Self-reviewed; no speed guarantee outside the measured workload.

## DT01: experimental direct coordinates in sealed postings (2026-09-22)

Before implementation, re-read PostgreSQL 18 index-scanning/index-locking and
immutable upstream 724edf9bde9d356724ad384a2e196edc3c9f80f7
`src/backend/access/heap/vacuumlazy.c`: lazy_vacuum calls index bulk deletion
before lazy_vacuum_heap_rel permits LP_DEAD slot reuse. Cleanup/compaction occurs
later and cannot substitute for bulk deletion. Existing MVCC-only bitmap reads
retain heap snapshot checks, including HOT. Generic WAL commits still use the
existing synchronous private-page API and at most three pages per record.

The experimental tag-9 page contains copied heap coordinates and monotonically
cleared local live flags beside ordered incarnation-qualified owner references.
Compaction publishes these only from live, published canonical owners while
holding the existing exclusive structural and writer barriers. A bulk-delete
pass must clear all copies of removed owners before returning. Interrupted
passes may leave stale copies but cannot authorize heap reuse; retries clear
copies from durable canonical liveness even when no new owner is removed.
This duplicates liveness per term and increases maintenance work; it is a
measurable bridge toward shared per-segment liveness, not the final layout.

Ordinary sealed and mutable pages remain supported by this binary. Older
binaries reject the new page tag; REINDEX with direct segment creation disabled
is required before downgrade. This experiment is default-off. No new unsafe
operation, host pointer, visibility shortcut, or external dependency is added.
Safe checked slice operations and existing checked delta codecs bound all page
work. pgrx 0.19.2 static SUSET GUC registration follows AM02. Cargo.lock is fixed.

Required evidence: codec corruption/truncation, Boolean oracle equality, mixed
mutable/direct chains, forced heap-coordinate reuse, interrupted bulk deletion
and compaction at every durable stage, PostgreSQL mutation/recovery tests and
matched SQL benchmarks. Self-review only; experimental status remains explicit.

## DT02: direct term iteration and endpoint proof (2026-09-22)

The tag-9 page adds first and last owner identities to the fixed header. Page
validation checks their equality with the decoded stream and enforces strict
owner/page/slot and incarnation order. A single-term scan checks the boundary
between pages, reads local live TIDs from direct pages, and resolves owners on
mutable and ordinary sealed pages. The existing Boolean executor continues to
intersect owner identities. These operations use safe Rust slices and checked
arithmetic; they add no host pointers or unsafe calls. Exact bitmap flags still
certify only predicate membership, never PostgreSQL visibility.

The count and parallel-work paths recognize direct pages as sealed sources but
continue to reread canonical owners under their existing VM/count protection.
The existing default-off count gates and PostgreSQL AM contracts remain.
Required review: pure corruption and query-oracle tests, small SQL mutation and
lossy bitmap tests, hard postmaster crash/replay, matched SQL latency and index
size. Self-reviewed until independent storage/FFI review is recorded.

Local evidence on `dbae7afd376e083e7e2c580d617ac682047bb76c` is archived in
`docs/runs/2026-09-22-direct-segments/`: pure Rust suite, 63 Python tests,
release Clippy, G2, normal-package G8 (234/234), hard postmaster replay and
matched SQL results with row-identity checks all passed. The direct SQL case
includes mutation; pure tests cover corruption, mixed chains, forced TID reuse
and interrupted durable stages. The final small warm fixture beats GIN only
for the common-term count. An attempted TID-only AND/OR merge failed an owner
incarnation oracle and was reverted. Independent storage/FFI review, write-load
and replica evidence remain open.

## OE01: owner-aware direct-page seek pruning (2026-09-22)

PostgreSQL 18's [index-scanning contract](https://www.postgresql.org/docs/18/index-scanning.html)
requires `amgetbitmap` to preserve candidate TIDs while the heap remains
responsible for visibility. The immutable PostgreSQL 18.6 source reviewed for
DT01 remains `724edf9bde9d356724ad384a2e196edc3c9f80f7`; no AM, FFI,
VACUUM or WAL boundary changes here. The on-disk tag-9 direct page from DT02
has validated first/last owner endpoints and a complete decoded-payload check
on every load. A Boolean seek may omit a second decode only when the page's
last owner is before the target (or equal for an exclusive seek). It advances
the previous owner to that validated endpoint, then checks the next page's
ordering and incarnation as usual. Copied TIDs are read only for surviving
owners. A pure owner oracle and SQL row-identity comparisons remain mandatory.

The selective AND benchmark and tests are archived under
`docs/runs/2026-09-22-owner-merge/`. The matched Pin median rose from 4,462.78
to 7,022.44 QPS on the 20,000-row fixture; GIN remained much faster. Pure Rust,
G2, Python, Clippy, normal-package G8 (234/234), and direct-page hard recovery
checks passed. Self-reviewed; no generalized speed or production claim.
Generation-safe page-group masks remain a separate design and correctness gate.

## GS01: generation-safe logical groups (2026-09-23)

Modules: `pin-core/src/grouped/` and `pin-kernels/src/grouped.rs`.
Authority: Rust 1.98.1 (`48a229cea`) checked slices, `as_chunks`, little-endian
integer conversion, bit operations, `array::from_fn` and `Iterator::min_by_key`.
Versioned official links, the byte layout, bounds and scalar/merge correctness
arguments are recorded in [g9-grouped-storage.md](g9-grouped-storage.md).
The PostgreSQL 18.6 and pgrx 0.19.2 immutable references above remain unchanged;
this change adds no host API, PostgreSQL pointer, allocation or unsafe operation.

Local obligations: one immutable incarnation per coordinate within a complete
segment; owner-checked term sealing; no cross-segment partial Boolean matching;
clear-only shared liveness; source-local liveness filtering before merge union;
fresh durable host identities and consistent private source snapshots. A full
membership/liveness open is eager and is not an I/O-pruning performance proof.

Evidence: independent incarnation-set oracle, scalar truth tables, malformed and
unaligned records, explicit TID reuse, checked sealing, logical merge conflicts,
cancellation, private-image retirement replay, decoding counters and a golden
wire image. Executable results are recorded by exact commit in PR #13 and its
G6 CI artifacts. Self-reviewed only. Physical extent publication, WAL/VACUUM
integration and default-off SQL activation were open in the logical-only slice.
The subsequent physical implementation and adapter are described in `G9PG01` below;
PostgreSQL crash qualification and independent review remain open.


## G9PG01: grouped PostgreSQL adapter (2026-09-23)

Scope: `pin-pg/src/grouped.rs`, `cshim/pin_grouped.[ch]`, grouped AM hooks,
`mutable/grouped/`, `mutable/page_grouped.rs`, G9 SQL/qualification drivers and
workflow gates. The base for this adapter work is PR #13 head
`f04ed8047ad331b084b340fbe99de5a98b056ae5`, not the earlier logical-foundation ZIP.
The pinned dependency graph and `Cargo.lock` are unchanged.

### Primary contracts reviewed

PostgreSQL source is pinned to
[`724edf9bde9d356724ad384a2e196edc3c9f80f7`](https://github.com/postgres/postgres/tree/724edf9bde9d356724ad384a2e196edc3c9f80f7),
PostgreSQL 18.6. pgrx/pgrx-pg-sys 0.19.2 source is pinned to
[`70383e884582d1bcc7cd681d10886b995a2830cb`](https://github.com/pgcentralfoundation/pgrx/tree/70383e884582d1bcc7cd681d10886b995a2830cb).

| Authority | Local requirement |
| --- | --- |
| [PG tuplesort.h](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/utils/tuplesort.h) | Exact datum-sort API signatures, KiB budget, `TUPLESORT_NONE`, forward-only access and sort-space instrumentation |
| [PG tuplesortvariants.c](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/utils/sort/tuplesortvariants.c) | Input copying before the stack datum is reused; `copy=false` output is borrowed only until the next sort operation |
| [PG configure.ac](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/configure.ac) | Compile the C adapters with the pinned aliasing, signed-overflow and floating-point precision semantics: `-fno-strict-aliasing`, `-fwrapv`, `-fexcess-precision=standard` |
| [PG varatt.h](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/varatt.h) | Include the header explicitly; use official varlena macros, validate uncompressed four-byte framing and exact payload width |
| [PG pg_operator.dat](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/catalog/pg_operator.dat) | Bytea ordering via generated `ByteaLessOperator`, not a handwritten or locale-dependent comparator |
| [PG miscadmin.h](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/miscadmin.h), [autovacuum.h](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/postmaster/autovacuum.h) | Maintenance-memory globals, worker-kind check, cancellation and autovacuum override |
| [PG resource budgets](https://www.postgresql.org/docs/18/runtime-config-resource.html) | Reserve core buffers separately; do not charge the whole server budget to a concurrent sort as well |
| [PG index functions](https://www.postgresql.org/docs/18/index-functions.html) | AM build/bulk-delete/cleanup boundaries, maintenance-memory capability, bitmap accumulation and required rechecks |
| [PG index scanning](https://www.postgresql.org/docs/18/index-scanning.html), [index locking](https://www.postgresql.org/docs/18/index-locking.html) | Bitmap consumers are MVCC-only; all applicable index copies must retire before heap reuse; multiple scan keys preserve complete predicate checks |
| [PG generic WAL](https://www.postgresql.org/docs/18/generic-wal.html), [generic_xlog.c](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/transam/generic_xlog.c) | Registered private images, exclusive locks through finish, standard `pd_lower`/`pd_upper` boundaries and bounded registration in lock order |
| [PG heapam_handler.c](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/heapam_handler.c) | Heap build supplies canonical HOT roots and handles recently dead HOT-chain versions; Pin must not reimplement that visibility logic |
| [PG VACUUM](https://www.postgresql.org/docs/18/sql-vacuum.html) | Run outside a transaction block; explicit cleanup/parallel settings in the disposable suite |
| [PG administration functions](https://www.postgresql.org/docs/18/functions-admin.html) | Observe advisory-lock waits, cancel the selected backend and inspect ordinary default-tablespace temporary files with `pg_ls_tmpdir` |
| [pgrx ffi.rs](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx-pg-sys/src/submodules/ffi.rs) | Each guarded closure is one C call with trivial captures, no panic and no destructor-bearing locals; backend main-thread restriction |
| [pgrx 0.19.2 GucRegistry](https://docs.rs/pgrx/0.19.2/pgrx/guc/struct.GucRegistry.html) | `define_bool_guc`, static settings, `Suset` privilege and default-off values |
| Rust 1.98.1 [slices](https://doc.rust-lang.org/core/primitive.slice.html), [NonNull](https://doc.rust-lang.org/core/ptr/struct.NonNull.html), [Vec](https://doc.rust-lang.org/std/vec/struct.Vec.html) | Checked chunks, exact record extents, non-null opaque handle, fallible scratch reservation and no allocation-free claim for host orchestration |

### FFI obligations

The five native operations are begin, batched put, finish, batched read and end.
`SortRecord` is `repr(transparent)` over `[u8; 32]`; compile-time Rust assertions
check size, alignment and the 256-record batch bound. The C datum has a separate
four-byte varlena header. Every output record is copied before another sort call.
The opaque handle is unique to one maintenance invocation and never enters the
pure core or PostgreSQL shared memory. All exceptional C calls use the existing
pgrx FFI guard, and no Rust destructor calls PostgreSQL.

An ordinary core `Result::Err` closes the sort before propagating. A PostgreSQL
ERROR is not such a return: context/ResourceOwner cleanup must reclaim sort and
tape resources as it unwinds through the guarded boundary. The native cancellation
suite is required to validate this assumption. The isolated C test doubles do not
model PG error cleanup, ABI compatibility, live spilling, WAL or buffer locking.

### Storage and performance obligations

The builder captures complete canonical owners, verifies duplicate live-coordinate
and coverage constraints, and never combines term fragments across generation
identities. The physical relation scopes numeric segment IDs. Its logical relation
namespace value `1` must not be interpreted as a global incarnation or used to mix
relations. Immutable metadata publication owns either the previous complete snapshot
or the complete replacement; unpublished/replaced fragments remain journal-owned.

Retirement is independent of the GUCs. It clears every published liveness copy
before canonical owner deletion and preserves the existing direct-posting cleanup.
No per-bit WAL flush, heap visibility shortcut, or unpinned shared-buffer borrow is
introduced. Plain scans and count paths are not switched to grouped storage.

The new scan's post-cutoff delta remains a conservative cover of all newer live
owners with predicate rechecks. The builder holds both barriers for a full snapshot
rebuild. Legacy postings are retained. These are known write-latency, read-delta and
space costs, not completed optimizations or measured gains. See
[g9-integration.md](g9-integration.md) for activation, rollback and benchmark gates.

### Evidence status

Self-reviewed for signatures, ownership, framing, memory-preflight, event numbering,
cutoff validity, fallback-before-output, retirement ordering and recovery schedules.
The existing PR-head CI supplied an exact rustfmt patch and identified Clippy issues;
those original-head issues were corrected before adding the adapter. Its earlier
successful Rust tests do not validate any later local commit.

The local suite passed 80 Python methods, including the production C bridge compiled
against test doubles in debug and optimized UBSan/bounds configurations. Source
contracts, shell/Python syntax and workflow YAML parsing also passed. Four new Rust
storage tests and the normal/test-hook PostgreSQL driver are present but have not
run natively for this adapter. G0/G9 now schedule native compilation, Rust debug and
release tests, scalar/no_std checks, SQL identity oracles, observed spill, cancellation,
recovery and concurrency. No run result is invented for these local commits.

Independent FFI/storage review, current-head rustfmt/Clippy/rustdoc, native SQL and
hard-crash qualification, grouped parallel-worker qualification, Miri/sanitizers for
Rust where applicable and matched performance measurements remain acceptance gates.
Both settings must remain default-off while those gates are open.

## Issue 14: bounded scan work and phrase rechecks

Base reviewed: `1d80b58e0eb17b003326283f8050ac3556f80776`. This candidate changes
safe query execution and evidence tooling, not the page format, publication/WAL
protocol, PostgreSQL ABI, SQL signature, dependency graph or Cargo lockfile.

| Official contract/source | Local obligation and evidence |
| --- | --- |
| [PG18 index scanning](https://www.postgresql.org/docs/18/index-scanning.html) and [locking](https://www.postgresql.org/docs/18/index-locking.html) | Exact predicate flags never bypass heap visibility. Retain the structural barrier, generation-qualified owners, unconditional retirement and whole-operation error behavior. G9 identity/fault tests remain required. |
| [PG `ginget.c`, immutable revision](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/gin/ginget.c) | `startScanKey` uses frequency-aware required/additional entries; `entryLoadMoreItems` distinguishes adjacent stepping from a new tree seek. These are research references, not copied locking assumptions. PIN uses its own immutable catalog and a proven conservative group bound. |
| [PG `tidbitmap.c`, same revision](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/nodes/tidbitmap.c) | Preserve sorted batched TIDs, per-page recheck accumulation and PostgreSQL-owned lossification. No private bitmap structure is accessed. |
| [Rust slice implementation at `48a229cea`](https://github.com/rust-lang/rust/blob/48a229cea/library/core/src/slice/mod.rs) | Eight-byte chunks are complete arrays and the tail is shorter than eight. Decode with `from_le_bytes`; the grouped tests cover offset widths and tails. |
| [Rust Vec documentation](https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve_exact) | Reservation is fallible and capacity may exceed the request. Keep retained capacity charged by the existing memory-budget helpers. |
| [pgrx 0.19.2 `pg_extern`](https://docs.rs/pgrx/0.19.2/pgrx/attr.pg_extern.html) and the existing [pgrx source pin](https://github.com/pgcentralfoundation/pgrx/tree/70383e884582d1bcc7cd681d10886b995a2830cb) | `matches(body: &str, bytes: &[u8]) -> bool`, immutable/strict/parallel-safe declarations and the existing error adapter remain unchanged. No PostgreSQL datum or text reference is retained. |
| Existing `unicode-segmentation = 1.12.0` lock and G1 profile contracts | Reuse the exact existing `unicode_words` and budgeted Unicode normalizer. The ASCII path relies on ASCII folding preserving word boundaries and byte lengths. |
| [Cargo test](https://doc.rust-lang.org/cargo/commands/cargo-test.html) | Keep `--locked`; run debug/release core/kernel tests, examples, formatting and Clippy in CI. Workflow definitions are not successful runs by themselves. |
| [PG18 EXPLAIN](https://www.postgresql.org/docs/18/sql-explain.html), [pgbench](https://www.postgresql.org/docs/18/pgbench.html), [psql](https://www.postgresql.org/docs/18/app-psql.html), and Linux [/proc](https://docs.kernel.org/filesystems/proc.html) | Preserve raw evidence, distinguish inclusive buffer/elapsed counters from CPU, pair GIN controls, validate backend identity, and reject inadequate CPU-tick resolution. |

### Review and qualification status

The bounded phrase matcher handles only one phrase node with 1..64 terms and keeps
at most 64 borrowed token references. Unsupported shapes keep the document oracle.
The PostgreSQL wrapper still decodes each query datum and does not cache a borrowed
datum or unsafe Rust object across calls.

The issue-14 finite models check conservative Boolean pruning and phrase-window work
accounting independently. SQL tests cover phrase truth values, Unicode cases, invalid
tails and prepared-statement reuse. The dedicated workflow runs formatting, locked
debug/release tests, Clippy and the phrase ablation.

Local model/C-test evidence predates the current PR head. Current-head CI and live
PostgreSQL measurements must be evaluated separately. No speedup, hardware-counter
result, allocation profile or production-readiness claim follows from these files.

## G9-FRONTIER01: exact term-addressed suffix membership

Review date: 24 September 2026. Modules:
`mutable/grouped/frontier.rs`, `mutable/grouped/scan.rs`,
`mutable/grouped/storage.rs`, and `mutable/page_grouped.rs` in `pin-core`.
Baseline: PR #15, `5afca59199b8ed6ddc3ce6c652d1d4d680562b76`.

Authority: PostgreSQL 18.6 [index scanning and rechecks](https://www.postgresql.org/docs/18/index-scanning.html),
[index locking](https://www.postgresql.org/docs/18/index-locking.html),
[VACUUM](https://www.postgresql.org/docs/18/sql-vacuum.html),
[generic WAL](https://www.postgresql.org/docs/18/generic-wal.html),
[visibility maps](https://www.postgresql.org/docs/18/storage-vm.html), and
[CustomScan](https://www.postgresql.org/docs/18/custom-scan.html). Pinned upstream
source remains `724edf9bde9d356724ad384a2e196edc3c9f80f7`:
[buffer access rules](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/storage/buffer/README),
[GIN scan](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/gin/ginget.c),
[index-only scan ordering](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/executor/nodeIndexonlyscan.c),
[bitmap heap execution](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/executor/nodeBitmapHeapscan.c),
[visibility map implementation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/visibilitymap.c), and
[generic WAL implementation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/transam/generic_xlog.c).

The AM supplies all matching index identities, not snapshot-visible heap tuples.
Clearing a recheck flag requires exact predicate membership. `amgetbitmap`
cannot itself return index-only tuples. `nodeIndexonlyscan.c` explains why VM
loads depend on ordering from index-buffer synchronization and snapshot
acquisition. This change does not establish a new equivalent VM protocol and
therefore does not bypass heap visibility or change the count/CustomScan gates.

Local proof obligations:

1. Complete grouped publication reserves an incarnation later than every
   captured owner, under the existing structural/writer protocol. No posting at
   or below that fence is a new frontier owner. Captured dictionary tails stop
   a reader from following an unbounded sequence of appends.
2. Owner references are ordered by stable owner page/slot and monotonically
   increasing incarnation. A tail whose final reference precedes the target
   proves exhaustion. A tail jump is safe only when its first reference is at
   or before the target, or it is the only page; otherwise walk from the head.
3. Boolean membership uses the full owner incarnation, never TID alone. AND
   cannot join two versions occupying the same heap slot. Canonical publication
   and liveness must be checked before emitting an owner. Unbounded NOT uses the
   published owner universe after the fence, not an arbitrary complement.
4. Every row visible to the captured PostgreSQL snapshot completed insertion
   before the scan. Concurrent appends may add invisible candidates; PostgreSQL
   filters them. VACUUM remains the authority for safe owner retirement, and
   the structural barrier prevents source reclamation during a grouped scan.
5. An exact Boolean result permits `recheck=false` only for that index
   predicate. Heap/HOT visibility, multiple-key rechecks, lossy bitmap handling,
   and SQL executor quals remain in the existing host adapter. A core candidate
   count is neither a visible SQL count nor necessarily a deduplicated count.
6. Interrupted/corrupt scans fail rather than return partial success. Existing
   C/pgrx error guards own buffer cleanup and bitmap invalidation. No new FFI,
   unsafe operation, page borrowing, WAL mutation or persistent format is added.

The buffer README requires a pin before accessing a buffer and a content lock
while examining page state. A pin alone is not a blanket immutable-byte lease.
`GroupNode` and parsed `Bitmap` views here borrow only checked private page or
scratch storage. The C shim still copies under its established pin/content-lock
contract. Reusing these Rust views does not extend any PostgreSQL pointer's
lifetime across a callback, unlock, cancellation or transaction boundary.

Rust authority: the official [Vec](https://doc.rust-lang.org/std/vec/struct.Vec.html)
and [slice](https://doc.rust-lang.org/std/primitive.slice.html) documentation
reports Rust 1.98.1, build `48a229cea` (1 September 2026), matching the selected
toolchain. `Vec::try_reserve_exact` is fallible and may receive more capacity
from the allocator than requested; it is not an exact RSS guarantee. The
program's 64-term limit bounds requested cursor storage. Existing grouped
payloads are released before frontier allocation. No per-hit vector growth is
used. `chunks_mut` establishes disjoint scratch regions; parsed immutable
views remain within those regions until the scan step ends. Checked addition
protects the incarnation target and candidate count. Safe slice/reader bounds
protect key/value access; `size_of` is used only for memory accounting, never
as an on-disk layout contract. The bare `pin-kernels` crate remains `no_std`;
the frontier itself uses bounded allocations in `pin-core`.

Evidence: `issue14_frontier.rs` covers 4,096 unrelated writes with owner-read
bounds, Boolean truth against the independent oracle, multi-page changed
suffixes, sealed/direct/mutable chains, TID reuse, every pre-publication insert
failure boundary, cancellation and fallback. CI run `35982647506` on
`62634401827211ea862156d6bef0bd19df01f8d3` passed all debug/release core/kernel
tests, the no-default-features kernel check and Clippy; formatting failed and
the exact CI formatter patch was subsequently applied. G9 run `35982647597`
passed. Initial PostgreSQL job `107577954174` in run `35982647514` passed
native lifecycle, grouped WAL recovery/concurrency and G2 transactional/recovery
qualification. The formatter-corrected code head `4db741005203b2462778ec7fac681477069e8ed9`
passed Issue14 run `35984784495`, G6 run `35984784501`, G9 run `35984784571` and
the G0 pure job. Its expanded native SQL suite was still running at this entry's
recording time. Evidence is not transferable to a changed head without rerunning
checks. Native SQL coverage is extended by `tests/sql/issue14_frontier.sql` in
the existing normal/test-hook qualification driver.

Performance evidence for this implementation: no local backend CPU or
throughput measurement. The previous CPU report predates PR #15. Read the
[decision and benchmark procedure](issue-14-frontier.md) for separate fresh,
unrelated-write, related-write, phrase, count, retrieval and maintenance gates.
`tools/issue14_frontier_bench.py` verifies the loaded revision and full result
identities before timing. Its guarded local-cluster setup and repeated
write/VACUUM phases preserve deferred GIN maintenance. Its PostgreSQL cursor,
repeatable-read and pgbench contracts follow the official
[DECLARE](https://www.postgresql.org/docs/18/sql-declare.html),
[isolation](https://www.postgresql.org/docs/18/transaction-iso.html),
[pgbench](https://www.postgresql.org/docs/18/pgbench.html), and
[Psycopg connection](https://www.psycopg.org/docs/connection.html) documentation.
Autocommit permits VACUUM outside transactions; explicit read transactions are
closed before maintenance; each worker owns its connection. Benchmark guards
and Python tests are not proof that its unrun live measurements succeeded.

Reviewer: implementation self-review only; independently assigned reviewer
still required. Remaining gates: final-head native SQL, recovery/standby
qualification, sustained-write measurements and related-suffix cost. No
20x/10x target or TIN-equivalence claim is authorized by this entry.

## G9FRONTIER02: opt-in persistent suffix anchors and executable activation

Review date: 24 September 2026. Supersedes G9FRONTIER01's no-new-format statement
only for the default-off anchor experiment. Implementation: `mutable/grouped/anchors.rs`,
`build.rs`, `frontier.rs`, `storage.rs`, `page_grouped.rs`, `compact.rs` and the
existing PostgreSQL grouped settings/adapter. Detailed protocol, cost model,
commands and qualification status: [snapshot frontier anchors](issue-14-anchors.md).

Official authorities retain the pinned PostgreSQL 18.6 source
`724edf9bde9d356724ad384a2e196edc3c9f80f7`:
[AM bitmap and recheck contract](https://www.postgresql.org/docs/18/index-functions.html),
[index locking and heap reuse](https://www.postgresql.org/docs/18/index-locking.html),
[generic WAL protocol](https://www.postgresql.org/docs/18/generic-wal.html),
[generic WAL implementation](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/transam/generic_xlog.c),
[buffer ownership](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/storage/buffer/README),
[custom option placeholders](https://www.postgresql.org/docs/18/runtime-config-custom.html),
and [registered settings](https://www.postgresql.org/docs/18/view-pg-settings.html).
The existing C copy/commit boundary retains its pin, content-lock, temporary WAL
image and finish/abort obligations. No borrowed PostgreSQL page or new unsafe
operation is introduced by anchor encoding, lookup or its test event.

Version-two metadata owns both catalog roots through one publication/recovery
journal. An anchor's term key, head, tail and terminal incarnation are checked
against the canonical dictionary and snapshot identity. Byte readers and explicit
little-/big-endian conversions define the format. Safe Rust
[slice operations](https://doc.rust-lang.org/core/primitive.slice.html) and
[Vec allocation](https://doc.rust-lang.org/alloc/vec/struct.Vec.html), reporting
Rust 1.98.1 (`48a229cea`), govern bounded borrows, chunking and fallible storage;
`size_of` remains a memory-budget calculation, never a serialization contract.

The anchor is valid only while canonical source pointers remain stable. The
existing exclusive structural barrier precedes the writer interlock. Compaction
must durably clear the validity bit before it can rewrite/recycle any source
page, even with every experiment disabled. Recovery must not accept a valid
anchor with a canonical rewrite journal. Readers retain the shared structural
barrier but not a writer barrier across cursor execution. Captured term tails
bound appends; full owner incarnations prevent cross-generation conjunctions;
heap visibility and HOT remain PostgreSQL's responsibility. Candidate counts do
not certify visible `COUNT(*)`. Old binaries must not read persisted v2 pages;
turning a GUC off is not migration.

pgrx authority is 0.19.2, commit
`70383e884582d1bcc7cd681d10886b995a2830cb`:
[GUC source](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx/src/guc.rs),
[versioned GUC implementation](https://docs.rs/pgrx/0.19.2/src/pgrx/guc.rs.html), and
[install command](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/cargo-pgrx/src/command/install.rs)
(the install source blob fetched during review is
`540ccd1f2a389e5600d99e9e8cdd8c71ac464c0e`). Static GucSetting/strings and Suset
registration preserve the existing lifetime and privilege contract. Stage 39 is
post-invalidation; stage 40 is a validated seek probe. Both reuse the existing
privileged test-hooks feature and ordinary host error/cancellation cleanup.
Normal PgStore event handling is a no-op.

Benchmark contract: `PGOPTIONS` and explicit SET must propagate the requested
flag to parent, child, row-retrieval, CPU and paired-snapshot connections. The
`pg_settings` registration check occurs after snapshot import, because a query
before import would violate the transaction protocol. An absent setting is
accepted only for a disabled old-binary control. Unknown custom settings alone
cannot prove the candidate implementation is loaded.

Cargo authorities: [build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html),
[environment overrides](https://doc.rust-lang.org/cargo/reference/environment-variables.html),
and [locked build](https://doc.rust-lang.org/cargo/commands/cargo-build.html).
Both `CARGO_TARGET_DIR` and `CARGO_BUILD_BUILD_DIR` must be fresh for each candidate;
empty `RUSTC_WRAPPER` and `RUSTC_WORKSPACE_WRAPPER` override configured wrappers.
The pinned cargo-pgrx install Args has no `--locked` option. The runner therefore
uses an explicit locked release prebuild, followed by install and a byte-for-byte
lockfile comparison before any measurements. Source tree/archive and copied
installed-library hashes supplement, not merely repeat, the revision stamp.
Failed runs and completed logs are checksummed after logging finishes.

Observed validation: the initial PR head `2ab08406e7bb8670aa2faf4375ef4f93a668bb53`
passed its nine core anchor tests, core Clippy and documentation in G0 run
`36008246679`; formatting failed and the exact CI formatter patch was retained
and applied. Its native tests left anchors off. Follow-up code commit
`0bf376fd766b7c9312941f26edcbb18ec04d6d28` passes 117 local Python tests, source
contracts, Python compilation and shell parsing. Python source/mocked-backend
checks do not establish Rust compilation or SQL/replay correctness. Two added
Rust tests and the new normal/test-hook anchored native matrices remain unrun
in this environment. See the retained [follow-up evidence](runs/2026-09-24-pr19-follow-up/README.md).

Reviewer: implementation self-review only. Required gates: exact-head native
compilation/formatting/tests, independent persistence and lock review, replay on
standby and migration/downgrade testing, baseline-versus-head isolated builds,
backend profile attribution, update/sustained-write/maintenance costs, and
latency distributions. No new-head backend performance measurement, 10x result,
or TIN comparison has been observed.

## G9FRONTIER03: one-pass dense owner payload frontier

Review date: 24 September 2026. Implementation:
`mutable/grouped/frontier.rs`, `mutable/document.rs`, `mutable/page_grouped.rs`,
`mutable/grouped/scan.rs`, the PostgreSQL grouped GUC/adapter, stage 41 test
hooks, qualification drivers, and [one-pass dense owner frontier](issue-14-owner-frontier.md).
This is a default-off read-path experiment. It changes no persistent page,
manifest, migration, or WAL format.

PostgreSQL authority is version 18:
[index scan candidate and recheck behavior](https://www.postgresql.org/docs/18/index-scanning.html),
[index AM bitmap callbacks](https://www.postgresql.org/docs/18/index-functions.html),
[index locking and heap-slot reuse](https://www.postgresql.org/docs/18/index-locking.html),
[bitmap heap execution](https://www.postgresql.org/docs/18/indexes-bitmap-scans.html),
[VACUUM](https://www.postgresql.org/docs/18/sql-vacuum.html), and the pinned
PostgreSQL 18.6 source commit `724edf9bde9d356724ad384a2e196edc3c9f80f7`:
[buffer access rules](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/storage/buffer/README),
[GIN bitmap scan reference](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/gin/ginget.c), and
[bitmap heap executor](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/executor/nodeBitmapHeapscan.c).
The AM returns candidate TIDs, not snapshot-visible rows. Exact index predicate
membership can clear that predicate's recheck bit, but it cannot certify heap
visibility, HOT identity, other keys, executor quals, RLS, or an index-only
count. This implementation preserves the existing PostgreSQL bitmap heap path.

pgrx authority is 0.19.2 at commit
`70383e884582d1bcc7cd681d10886b995a2830cb`:
[GUC implementation](https://docs.rs/pgrx/0.19.2/src/pgrx/guc.rs.html) and
[GUC source](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx/src/guc.rs).
A static `GucSetting<bool>` is registered as SUSET with boot value false. The
value is sampled through the operation's `PageStore`; no PostgreSQL pointer or
setting reference enters `pin-core`.

Rust authority is the official Rust 1.98.1 documentation for
[`Vec::try_reserve_exact`](https://doc.rust-lang.org/1.98.1/std/vec/struct.Vec.html#method.try_reserve_exact),
[`Vec::capacity`](https://doc.rust-lang.org/1.98.1/std/vec/struct.Vec.html#method.capacity),
[`usize::checked_mul`](https://doc.rust-lang.org/1.98.1/std/primitive.usize.html#method.checked_mul),
and [slice indexing](https://doc.rust-lang.org/1.98.1/std/primitive.slice.html).
`try_reserve_exact` is fallible and may receive more allocator capacity than
requested. The implementation checks actual capacity against the operation
budget. Checked conversion, addition, multiplication, subtraction, and slice
bounds protect the output and fragmented-payload buffers. No unsafe Rust or
layout-dependent serialization is added. `pin-kernels` remains allocation-free
`no_std`; this bounded path is in `pin-core`.

Local proof obligations:

1. The complete grouped snapshot's owner fence precedes every eligible frontier
   owner. The scan starts at the next owner slot, requires strictly increasing
   owner coordinates and incarnations, and stops above the allocation ceiling
   captured from the same metapage. A later writer may append to the same owner
   page, but that post-capture incarnation cannot expand this scan's work set.
2. Eligibility is default off, requires at least 512 reserved incarnations, and
   requires either an owner-universe query or two changed query terms. Captured
   posting tails are validated before this decision.
3. Only published and live owners contribute membership. The full owner
   incarnation and original root TID remain the identity. TID alone never joins
   terms from different heap-slot incarnations.
4. Prepared-document membership checks profile ID, owner token/term metadata,
   envelope lengths, UTF-8, strict term ordering, positive position counts,
   total token counts, and complete payload consumption. Boolean membership
   deliberately does not decode positional deltas. Position-dependent query
   shapes retain the established fallback.
5. Fragment chains must retain one owner reference, contiguous offsets, exact
   total bytes, bounded block traversal, and complete termination. Scratch is
   reused and cannot exceed the remaining memory budget.
6. All matching root TIDs are buffered before the first emit. Insufficient
   budget or an optional vector reservation failure returns `None` and selects
   the canonical term-addressed frontier. No fallback follows partial
   owner-frontier output.
7. The existing shared structural barrier and host buffer-copy contract protect
   every page read. The implementation adds no borrowed PostgreSQL page lifetime,
   WAL transition, maintenance publication, or reclamation rule.
8. Cancellation and corruption fail through the existing host guard. Stage 41
   exists only under the privileged test-hook contract and normal stores ignore
   it. PostgreSQL discards partial bitmap state on the error path.

Qualification: pure-engine tests cover exact identities across a 1,024-owner
related delta, two-tail activation, unrelated-delta avoidance, fail-before-emit
memory fallback, and a writer appending a post-capture incarnation into the same
physical owner tail before owner evaluation begins. The native qualification constructs another 1,024-owner
related delta, checks a sequential heap oracle for AND/OR/NOT, performs HOT-
eligible and indexed updates, DELETE and VACUUM, pauses stage 41 while a writer
commits, verifies statement-snapshot identities, performs immediate restart,
and reruns the oracle. Existing grouped suites retain exact heap-slot reuse,
maintenance error boundaries, WAL replay, cancellation, and concurrent VACUUM.

Local development evidence for the documentation/tooling follow-up is 123
Python tests, Python compilation, shell parsing, source contracts, and diff
whitespace checks. The local container has no Rust/PostgreSQL 18 qualification
toolchain. These checks do not establish Rust compilation, native SQL success,
recovery success, or performance. Final-head CI and live benchmark artifacts
must be evaluated separately. No owner-frontier speedup, GIN multiple, write/WAL
result, production-readiness result, or TIN comparison has been observed.

Reviewer: implementation self-review only. Remaining gates are independent
identity/visibility review, exact-head Rust and PostgreSQL matrices, paired
activation-off/on profiles, all query-class regressions, sustained writes,
fragment-heavy documents, allocation/RSS evidence, write CPU, maintenance CPU,
WAL bytes, cold I/O, and direct TIN measurements before any parity statement.

## COUNT03: grouped exact COUNT generation interlock

Verified against the named sources on 2026-09-25. Local implementation:
`mutable/grouped/scan.rs`, `pin-pg/src/grouped_count.rs`, `cshim/pin_count.c` and
`pin_count.h`. [Design, path trace and proof obligations](g9-grouped-count.md).
This is implementation self-review, **not independent approval**. The new
superuser GUC remains default off. Rust/native PostgreSQL compilation, isolation,
recovery and performance results have not been observed for these changes.

### Immutable contracts read and rechecked

The six requested PostgreSQL 18 manual sections were read: [AM functions](https://www.postgresql.org/docs/18/index-functions.html),
[index scanning](https://www.postgresql.org/docs/18/index-scanning.html),
[index locking](https://www.postgresql.org/docs/18/index-locking.html),
[CustomScan](https://www.postgresql.org/docs/18/custom-scan.html),
[VM](https://www.postgresql.org/docs/18/storage-vm.html), and
[generic WAL](https://www.postgresql.org/docs/18/generic-wal.html).
The mutable manual URLs are explanatory references, not immutable ABI authority.
Exact code authority is PostgreSQL 18.6 commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7`:

| Boundary and pinned upstream source | Local obligation and test/gate |
| --- | --- |
| [`nodeIndexonlyscan.c`, especially the VM memory-ordering argument](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/executor/nodeIndexonlyscan.c#L120-L185) | Acquire source protection before reading membership and VM. Heap insert clears VM before index publication. Delete visibility depends on the PostgreSQL snapshot/VM ordering, not a process-local cached bit. New native concurrency and old-snapshot tests are written but unrun. |
| [`visibilitymap.c` ownership, locking and WAL notes](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/visibilitymap.c#L20-L86) | Use `visibilitymap_get_status`, retain/release its buffer through the existing resource-owning C state, test `VISIBILITYMAP_ALL_VISIBLE`, never conflate frozen and visible. VM-off forces heap fetch. Host-double tests verify fresh probes; actual VM/cache ordering is an independent native gate. |
| [`lmgr.c`, `ConditionalLockPage` and `UnlockPage`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/storage/lmgr/lmgr.c#L499-L552) | New shared page-0 lock conflicts with PIN's existing exclusive writer interlock. Acquire after structural share, never upgrade, and fall back on failed conditional acquisition. Set the ownership flag only after success, release exactly once; resource cleanup handles ERROR. Debug/UBSan actual-body tests cover rejection/idempotence; native deadlock and starvation gates remain. |
| [`tableam.h`, `table_index_fetch_tuple`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/access/tableam.h#L1181-L1236) | Dirty roots use the supported snapshot and HOT-aware callback, not `table_tuple_fetch_row_version`. Copy the root because the callback may modify it. Initialize `call_again=false`; reject unexpected continuation for MVCC. Keep fetch/slot initialized until release. Test double mutates TID and exercises domain/snapshot/continuation/errors; real HOT/visibility oracle is unrun. |
| [`vacuumlazy.c`, three-phase ordering](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/vacuumlazy.c#L6-L43) | Index retirement must precede freeing indexed heap slots. The group liveness clear precedes canonical owner retirement under the existing interlock. A structural snapshot reference alone is insufficient. Independent review must confirm dead-root/VM and HOT-redirect cases, multi-round VACUUM and concurrent maintenance. Native harness asserts actual CTID reuse with changed terms. |
| [Custom path](https://www.postgresql.org/docs/18/custom-scan-path.html), [plan](https://www.postgresql.org/docs/18/custom-scan-plan.html), [execution](https://www.postgresql.org/docs/18/custom-scan-execution.html) plus existing pinned COUNT01 planner/executor sources | Extend syntactic eligibility only; preserve retained ordinary AggPath and existing runtime security/snapshot/relation checks. Do not invent an `amcanreturn` capability. Recheck GUC on cached execution, clear counters on rescan, exclude parallel/recovery/SSI/unsupported predicates. G0 schedules full integration; local source tripwires do not prove host lifecycle. |
| Generic WAL manual and existing pinned G2/G9 storage ledger | No new page modification, WAL record, buffer borrow, liveness format, crash state or redo callback. Preserve registered-copy/exclusive-buffer contracts. Count protection is primary-side only; existing recovery-time index-read refusal remains. |

pgrx 0.19.2 authority is commit
`70383e884582d1bcc7cd681d10886b995a2830cb`:
[`pgrx-macros/src/rewriter.rs`](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx-macros/src/rewriter.rs)
and [`pgrx/src/guc.rs`](https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx/src/guc.rs).
New exported Rust callbacks use `#[pg_guard] extern "C-unwind"`; throwing C calls
are inside the existing `native::call` FFI guard with trivial scalar/pointer
captures. PostgreSQL pointers stay out of pin-core. GUC access is backend-thread
local through the audited wrapper, not a thread-safe work-queue configuration.
Nested C/Rust error/unwind and whole-library link compatibility require independent
review and native tests; the host doubles do not establish them.

Rust 1.98.1 authority is commit
`48a229ceaefd4985c50990b14116b6d856af0985`:
[`slice/raw.rs`](https://github.com/rust-lang/rust/blob/48a229ceaefd4985c50990b14116b6d856af0985/library/core/src/slice/raw.rs#L6-L37)
and [`num/uint_macros.rs`](https://github.com/rust-lang/rust/blob/48a229ceaefd4985c50990b14116b6d856af0985/library/core/src/num/uint_macros.rs#L61-L87).
Query bytes form one initialized immutable allocation retained by C for the
synchronous call; reject null and length above `isize::MAX`. No borrow survives
query/slot reset. Output pointers refer to one aligned `i64` and 18 exclusively
writable aligned `u64`s. Output is written only on complete success. The C/Rust
counter widths, arrays and older parallel participant memory change together.
Private C ABI is not a persistent format. All objects must be rebuilt together.

Scalar `count_ones` and the existing checked bit loop operate on validated offset
masks, with tail bits constrained by `HeapLayout`. No new ISA intrinsic or
`target_feature` promise is made. The compiler's popcount implementation is not
an assertion about native speed. Core code remains safe Rust. `try_reserve_exact`
and checked allocation/error contracts remain as recorded in the preceding
frontier ledger. Sink failure invalidates all partial work; `None` is available
only before emissions. Optional owner-frontier fallback happens within the existing
bounded frontier machinery, not by restarting the entire count after output.

Cargo authority for the selected Rust toolchain is submodule commit
`797e8a9bca276c1c9f9f738d2a20f484fa4eea9d`:
[lockfile contract](https://github.com/rust-lang/cargo/blob/797e8a9bca276c1c9f9f738d2a20f484fa4eea9d/src/doc/src/guide/cargo-toml-vs-cargo-lock.md)
and [profiles](https://github.com/rust-lang/cargo/blob/797e8a9bca276c1c9f9f738d2a20f484fa4eea9d/src/doc/src/reference/profiles.md).
The existing Cargo-generated lockfile, unwind profile, dependency set and pinned
Rust toolchain are preserved. Native CI remains responsible for `--locked` builds,
rustfmt, Clippy, debug/release tests and the full PostgreSQL matrix. No Rust
installation was attempted in the authoring VM.

### Evidence and unresolved gates

Independent input oracles: Rust page-mask tests enumerate the full coordinate
domain, compare complete root identity sets to the document evaluator, retain the
original bitmap adapter control, test sparse/dense/Boolean/NOT/empty membership,
short/long frontiers and retired-A/reused-B false-AND rejection. These tests are
written, not executed here. The finite Python protocol model uses a separately
supplied snapshot outcome, and its broken-protocol controls must find witnesses.
The new C bridge bodies compile/run against host doubles in debug and UBSan/bounds
modes; they cover stale lock state, rejection, private TID mutation, VM probing,
continuation and interruption. Neither suite substitutes for PostgreSQL.

Native normal/test-hook qualification is wired into G0 with durable settings and
independent PostgreSQL row identities. It adds generation-pause stage 42 and reuses
visibility stages 14/15, including cancellation after partial work. All hooks remain
privileged and absent from normal builds. Native expected results, crash/replay,
standby guard qualification, full memory-pressure/failure testing, hardware matrix,
performance and sustained writer latency are still open. The new benchmark rejects
unregistered placeholder GUCs, dirty worktrees and source/binary revision mismatch,
retains GIN in every balanced block and does not change ranking semantics.

Final observed local test counts, failed experiments and source hashes are in the
[run record](runs/2026-09-25-grouped-count/README.md). That record explicitly
separates native tests scheduled from tests actually executed. No 10x or TIN-parity
claim follows from this implementation.

## 2026-09-25 grouped sparse catalog and delta review

PostgreSQL 18.6 index AM, page, extension search-path, and WAL authority remains
the pinned source commit `724edf9bde9d356724ad384a2e196edc3c9f80f7`
listed above. The relevant reviewed contracts are
[`index-functions`](https://www.postgresql.org/docs/18/index-functions.html),
[`storage-page-layout`](https://www.postgresql.org/docs/18/storage-page-layout.html),
[`generic-wal`](https://www.postgresql.org/docs/18/generic-wal.html), and
[`runtime-config-client`](https://www.postgresql.org/docs/18/runtime-config-client.html).
Catalog entries and grouped pages remain private PIN payloads inside PostgreSQL
index pages. The host still owns buffer locks, generic WAL publication, index
scan rechecks, heap visibility, and VACUUM ordering. The new 18-posting inline
encoding changes no PostgreSQL pointer or FFI boundary. It packs sorted
heap-page/offset coordinates into 54 catalog bytes; decoding validates order,
page mask, offset domain, and zero padding before constructing the existing
checked bitmap view. It writes no separate posting page for these entries.
Large postings retain the previous page-backed format. Inline values use
`len = 0`, which older PIN binaries reject during read. An index using the new
format requires `REINDEX` with grouped storage disabled before binary downgrade.
The experimental grouped storage and delta seal GUCs remain default off.

Rust 1.98.1 `Result`, checked integer, slice bounds, and array contracts remain
those in the pinned Rust source ledger above. Cargo.lock and the pgrx 0.19.2
bindings are unchanged. The new codec uses safe Rust and fixed-size scratch.
Local pure tests cover sparse round trip, bitmap equivalence, corrupt page masks,
and overflow to the old format. PostgreSQL 18.6 native lifecycle and paired
performance qualification are required before enabling the format by default.

### Grouped dirty-page visibility batch

Exact PostgreSQL 18.6 authority is
[`heapam_handler.c`, `heapam_index_fetch_tuple`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/heapam_handler.c#L115-L160),
[`heapam.h`, HOT/prune prototypes](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/access/heapam.h#L1608-L1722),
and [`pg_bitutils.h`, set-bit iteration](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/port/pg_bitutils.h#L140-L169).
The supported heap AM and MVCC snapshot checks already precede the grouped
COUNT path. The new opt-in bridge consumes one validated eight-word offset mask
while the owner generation guard remains held, uses PostgreSQL's buffer manager,
prunes only on a buffer switch, takes one shared content lock, and calls the
same `heap_hot_search_buffer` routine as the heap AM for each root. It retains
the buffer pin in backend-local state until the next page or cleanup. It reads
no text datum and does not copy a heap tuple into a slot. The fallback keeps
`table_index_fetch_tuple`. The new GUC is superuser-only and default off.
Native old-snapshot, HOT, VACUUM, abort, crash, and paired CPU tests are required
before this bridge can be enabled by default. An independent FFI/locking review
remains open.

### Inline positional phrase proof

Review date: 26 September 2026. Baseline: merged PR #26,
`c90df9c8166c55ad19ae1045aed9305c30e27c2d`. Modules:
`mutable/document.rs`, `mutable/query.rs`, `mutable/grouped/scan.rs`, and
`pin-pg/grouped.rs`. Exact PostgreSQL 18 authority:
[index scanning and recheck semantics](https://www.postgresql.org/docs/18/index-scanning.html),
[index access method callbacks](https://www.postgresql.org/docs/18/index-functions.html),
and pinned upstream
[`tidbitmap.c`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/nodes/tidbitmap.c),
[`nodeBitmapHeapscan.c`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/executor/nodeBitmapHeapscan.c),
and [`heapam_handler.c`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/access/heap/heapam_handler.c#L115-L160).
The access method must return all matching TIDs. A false recheck flag is valid
only when index membership is exact; heap MVCC visibility remains PostgreSQL's
responsibility. Bitmap scans do not support index-only tuple delivery.

The opt-in `pin.enable_phrase_positions` GUC is superuser-only and default off.
Only a root-level phrase of at most 64 normalized terms is eligible. The
existing posting intersection finds candidates. For each candidate, PIN reads
the current published, live owner at its full incarnation and validates the
complete inline PD02 positional payload against its token and term counts.
Success emits that root with `recheck=false`; absence emits nothing. A
fragmented owner or a plan-budget fallback keeps `recheck=true`. The host's
existing `pin.enable_exact_bitmap` gate, multiple-key rule, and bitmap sink
still decide the final recheck flag. PostgreSQL retains HOT-chain and snapshot
checks on every heap visit. There is no new FFI, unsafe operation, persistent
format, WAL path, or heap visibility shortcut.

Proof obligations: the indexed token normalization and position numbering must
equal the SQL `pin.matches` phrase oracle; owner publication and incarnation
must prevent a cross-version positional proof; every emitted exact root must
match the whole predicate; and every unproven path must retain a heap recheck.
Pure tests compare the indexed proof with the independent text oracle,
including repeated terms and Unicode, and cover fragmented fallback. The
26 September native run compared full heap identities across HOT, indexed
update, rollback, delete, VACUUM, and REINDEX; a paired CPU run measured the
phrase benefit. Concurrent writers, old snapshots, crash recovery, standby
replay, and independent review remain gates before default enablement.

The CPU-clock profile of the first phrase implementation showed separate
`document::validate`, `DocumentTerm::positions`, and `phrase_matches` work on
each candidate. The follow-up `validate_inner` callback records selected
position views while it validates every term and every token once; the public
`validate` API delegates to the same validation loop. The callback's selected
views are used only after validation and expected token/term count checks
succeed. This changes neither PD02 bytes nor the corruption rule. On the
replayed fixture, full identities matched across PIN old, PIN one-pass, and
GIN; backend CPU for `"bravo charlie"` fell from 27.53 to 22.47 ms. A native
repeatable-read snapshot held its full result stream across committed writes.

### Rejected relation-size syscall experiment

Review date: 26 September 2026. Authority: pinned PostgreSQL 18.6
[`md.c`, `mdnblocks`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/storage/smgr/md.c),
[`bufmgr.c`, `ReadBufferExtended`](https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/backend/storage/buffer/bufmgr.c),
and [index locking](https://www.postgresql.org/docs/18/index-locking.html).
A trace of 40 warm phrase queries on the merged baseline showed 50,320
`lseek` calls and no file reads in the backend. `RelationGetNumberOfBlocks`
calls from the Rust page bound and C page reader were the cause. A callback
size cache followed by a bounded C page-read entry reduced the traced `lseek`
count to 200 for 40 queries. Paired backend CPU for `"bravo charlie"` was
27.53 ms with positional proof before the cache versus 27.59 ms with both
syscall changes; the legacy path stayed near 79 ms. Neither CPU result supports
retaining a new FFI and concurrency proof burden. Both changes were reverted
before this candidate was proposed. The trace scripts and raw measurements
remain as evidence that syscall count alone is a poor proxy for query CPU.


## SELPOS01: selected positional views and fragmented phrase proof

Contracts reviewed: PostgreSQL 18 index scanning/locking (MVCC bitmap scans
continue through heap visibility), and the repository's pinned PostgreSQL
`724edf9bde9d356724ad384a2e196edc3c9f80f7` contract already recorded above.
Local `storage::with_reader` retains `pin_structure_lock(index, false)` across
the pure scan; reclamation requires its exclusive counterpart. Views borrow
owned page copies or a query-owned fragment buffer, never unlocked PG memory.
No pgrx, C, WAL, AM capability or unsafe boundary changes are introduced.

Rust `Vec::try_reserve_exact` can provide more capacity than requested: the
reader checks actual capacity and falls back on allocation/budget failure.
The existing pinned Rust allocation contracts apply. The versioned online Rust
1.98.1 documentation was inaccessible during this review; no new foreign API
assumption relies on it.

Obligations: reject wrong owner/offset/chain/length, preserve publication and
liveness checks, retain MVCC and bitmap lossification rechecks, charge concurrent
plan and payload memory, retain the full integrity validator. Query validation
now intentionally omits unused positional deltas and global cross-term
uniqueness; `docs/g4-query-execution.md` states that policy explicitly.

Local independent-oracle coverage includes inline and fragmented phrases,
repeated terms, reversed/nonadjacent terms, selected/unselected corruption and
budget fallback. Native lifecycle and CPU results are recorded with the run.
This work is an A3 bridge from `pin_next.md`, not the new primary format, ranked
SQL execution, or a claim of TIN parity.

## PREFIX01: bounded phrase witnesses

This pure reader uses the existing owned-page and structural-barrier contracts
from SELPOS01. PostgreSQL 18 index scanning was re-read before this change:
removing predicate rechecks after an exact proof does not remove heap MVCC or
bitmap lossification obligations. No PostgreSQL, pgrx, Cargo, unsafe, WAL, or
storage format boundary changes occur. Rust checked slice/checked-add and the
existing canonical Reader::var_u32 contracts bound all prefix reads.

The reader validates consumed data and can stop before unneeded stream/fragment
tails. This observable corruption-policy change is documented in G4. A checked
witness proves predicate existence; it is not proof of complete index integrity.
The independent text oracle tests every byte prefix of generated documents;
physical-work tests prove one fragment read for early witnesses and multiple
reads for late terms. Required native qualification covers the existing phrase
lifecycle matrix and the stronger stored-vector GIN control.

Design references (rechecked):
https://planetscale.com/blog/anatomy-of-a-postgres-search-engine
https://www.postgresql.org/docs/18/index-scanning.html

## POSBLOCK01: independently seekable position experiment (2026-09-26)

Scope: `pin-core::codec::position_blocks`, its oracle and in-memory benchmark.
No PostgreSQL/pgrx/FFI/unsafe/WAL/native-format boundary changes. Existing Reader
and Writer canonical varint, checked slice and integer contracts apply. The wire
contract and integrity distinction are in `docs/position-blocks.md`.

Official architecture reference re-read:
https://planetscale.com/blog/anatomy-of-a-postgres-search-engine (2026-09-22).
It supports separating membership and positional data; PB01's 128-position block
and directory design are our experiment, not a claim about private TIN code.
PostgreSQL 18 index locking was re-read:
https://www.postgresql.org/docs/18/index-locking.html.
Pinned PostgreSQL source remains commit
`724edf9bde9d356724ad384a2e196edc3c9f80f7`; existing scan/visibility obligations
remain unchanged. There is no new PG function or binding to review here.

Review: safe borrowed slices and caller buffers only; checked count/extent/order
and bounded selected-block work. Tests compare every target around each generated
position against a sorted-vector oracle, cover empty/extreme counts, truncation,
output atomicity, mutated bytes and the selected/skipped corruption distinction.
Full verification is `validate_all`, not `open`. Native integration is explicitly
pending owner/lifetime/storage recovery qualification; kernel timing must not be
reported as SQL performance.
