# API evidence and local obligations

Verified against PostgreSQL 18.6 (`724edf9bde9d356724ad384a2e196edc3c9f80f7`),
pgrx 0.19.2 (`70383e884582d1bcc7cd681d10886b995a2830cb`), and Rust 1.98.1.
Review date: 17 September 2026. These are implementation contracts, not a claim
that the new boundary has passed compilation or runtime tests.

Every unsafe entry below still needs an independently assigned reviewer. No
reviewer is recorded as having approved it. G0 remains open.

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
inventory tests and deliberately mutated inventories. C/Rust compilation and
runtime comparison must run in CI; Python inventory checks do not replace them.

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
opclass, and continued backend operation. These authored tests have not run on
this branch. They do not prove buffer/ResourceOwner cleanup or backend-death
recovery, because G0 has not acquired those resources.

## BUILD01: native C compilation and matching host headers

Module: `crates/pin-pg/build.rs`.

Authority: [Cargo build scripts](https://doc.rust-lang.org/cargo/reference/build-scripts.html),
[`Command`](https://doc.rust-lang.org/1.98.1/std/process/struct.Command.html), and
[`pg_config`](https://www.postgresql.org/docs/18/app-pgconfig.html).

The script requires the absolute `PGRX_PG_CONFIG_PATH` used by pgrx, native
x86_64 Linux GNU, and PostgreSQL 18.6 headers. Compiler and archiver arguments are
passed as separate arguments, not shell commands. `CC` and `AR`, when set, must
be executable paths, not shell command strings. It checks every child exit code
and UTF-8 metadata result, writes only to OUT_DIR, and never downloads tools.
Cargo watches the selected pg_config, header tree, and local C inputs. The
existing dependency graph and Cargo-generated lockfile remain unchanged.

Evidence: manifest/source policy checks, CI C compilation, extension linking,
installation, and disposable cluster suite. No native Rust or PostgreSQL tool
is installed in the development VM.

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
