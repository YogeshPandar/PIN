# G6 API evidence and unsafe obligations

Reviewed official Rust 1.98.1 documentation, compiler source commit
`48a229ceaefd4985c50990b14116b6d856af0985`, on 19 September 2026.
This ledger records a self-review, not independent unsafe approval.

## KERNEL01: private AVX2 bitmap loops

Modules: `crates/pin-kernels/src/{lib,scalar,x86_avx2}.rs`.

Official interfaces:

- https://doc.rust-lang.org/std/sync/struct.OnceLock.html#method.get_or_init
- https://doc.rust-lang.org/std/arch/macro.is_x86_feature_detected.html
- https://doc.rust-lang.org/reference/attributes/codegen.html#the-target_feature-attribute
- https://doc.rust-lang.org/std/primitive.slice.html#method.as_chunks_mut
- https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_loadu_si256.html
- https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_storeu_si256.html
- https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_and_si256.html
- https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_or_si256.html
- https://doc.rust-lang.org/core/arch/x86_64/fn._mm256_andnot_si256.html

Immutable standard-library source:
https://github.com/rust-lang/rust/tree/48a229ceaefd4985c50990b14116b6d856af0985/library
The stdarch submodule at that commit supplies architecture intrinsics.

The public handle can select AVX2 only after runtime detection. The private
backend enum is not caller-constructible. Feature detection is cached per
process and occurs only during selection, not inside a word loop. Forced
unsupported modes fail; automatic selection falls back to scalar. Miri and
non-x86_64 builds omit the intrinsic module. Scalar remains explicitly forceable.

All lengths are compared before any output write. Immutable inputs may alias
one another, but the exclusive output borrow cannot overlap them in a valid
call. Every intrinsic receives exactly four initialized u64 elements in a live
slice chunk. Unaligned load/store relax vector alignment, not bounds or lifetime.
No shared PostgreSQL page is borrowed, no raw slice is constructed, and no pointer
escapes the call. Fewer than four remaining words use only scalar operations.
Difference reverses the intrinsic arguments because andnot computes NOT a AND b.

No allocator, PostgreSQL API, panic handler or synchronization callback runs in
the vector loop. Unsupported features cannot be enabled through a public bool
or unvalidated enum cast. Distribution builds must not set target-cpu=native.

Evidence authored: independent per-word differential tests over all three
operations, zero lengths, every vector tail, all u64 alignments modulo 32 for
both inputs and output, exact allocations, identical input aliases, densities
and every small length-mismatch permutation. Guards detect output clobbering.
Observed G6 run `35498740070` at `93c5bbeba0d53c064b6f55a9be4b51849493114c`
passed debug/release kernel and core tests, Clippy, rustdoc, the pure benchmark
examples, Miri scalar/tail checks, and AddressSanitizer kernel tests. These checks
do not prove PostgreSQL visibility or end-to-end performance.

## Performance gate

No production call site selects automatic SIMD yet. Kernel benchmarks are
experiments; controlled PostgreSQL benefit at equal durability/memory and
independent unsafe review are still required before default activation.

## CORE06: bounded query scratch and offset run counts

Modules: `pin-core/src/{memory,mutable/query,codec/offsets}.rs`.
Official contracts: Rust 1.98.1 `Vec::capacity`, `try_reserve_exact`, `drop`,
`u64::count_ones`, and `slice::as_chunks`/`as_chunks_mut` at the compiler commit
above. The typed chunk APIs return fixed-size arrays plus a remainder shorter
than four elements; they never pad or extend a borrowed allocation.

- https://doc.rust-lang.org/std/vec/struct.Vec.html#method.capacity
- https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve_exact
- https://doc.rust-lang.org/std/mem/fn.drop.html
- https://doc.rust-lang.org/std/primitive.u64.html#method.count_ones

Temporary mapped/active vectors are dropped before releasing their actual
capacity charges. Other live vectors remain charged; peak accounting is not
reset. Continuation storage is omitted only for roots already handled directly
by `Plan::seek`; complex roots still reserve the bounded stack fallibly.
No cursor, liveness, snapshot, buffer or WAL contract changes.

For each bitmap word, a run starts at a set bit whose predecessor is clear.
The previous word's high bit supplies bit zero's predecessor. The bounded
512-bit domain fits at most 256 runs and u16 arithmetic. Zero unused bits are
maintained by existing constructors and checked decoding. Size selection keeps
all headers and exactly the sparse/bitmap/runs tie order; the format is unchanged.
Default set operations remain scalar. `combine_with` is explicitly opt-in.

Evidence: `g6_offsets` compares encoded sizes and tags to a separate per-offset
counter for exhaustive small sets and every supported domain. Tests cover
cross-word boundaries, exact round trips, every kernel mode, and foreign-domain
rejection. Private allocation tests cover direct roots and peak preservation.
Debug/release tests, Clippy and rustdoc passed in G6 run `35498740070` at
`93c5bbeba0d53c064b6f55a9be4b51849493114c`. G0 run `35498740038` compiled the
PostgreSQL extension and passed installation/lifecycle and host-boundary linting;
its later qualification failure was traced to a misspelled fixture path and a
stale source-string assertion, both outside the compiled implementation.

## COUNTRECHECK06: default-off pgrx boolean GUC

Module: `pin-pg/src/count.rs`.

The exact pgrx release is 0.19.2, tag commit
`70383e884582d1bcc7cd681d10886b995a2830cb`. Its `pgrx/src/guc.rs` defines
`GucSetting<bool>::new(bool)` as a const constructor and
`GucRegistry::define_bool_guc` over a static `GucSetting<bool>`.
The constructor call is explicitly type-qualified because pgrx also provides
inherent `new` methods for several other `GucSetting<T>` specializations.

Immutable source:
https://github.com/pgcentralfoundation/pgrx/blob/70383e884582d1bcc7cd681d10886b995a2830cb/pgrx/src/guc.rs

`pin.enable_count_recheck` remains `PGC_SUSET` and false by default. The setting
selects only the exact single-term heap recheck implementation. It does not alter
snapshot choice, owner pinning, VM certification, HOT fetches or SQL semantics.
The sequential predicate retains the independently materialized document oracle.

## MEASURE06: benchmark and memory-tool boundaries

- https://doc.rust-lang.org/std/hint/fn.black_box.html
- https://doc.rust-lang.org/std/time/struct.Instant.html
- https://doc.rust-lang.org/unstable-book/compiler-flags/sanitizer.html
- https://github.com/rust-lang/miri/blob/master/README.md
- https://www.postgresql.org/docs/18/pgbench.html
- https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/doc/src/sgml/ref/pgbench.sgml

Benchmarks allocate fixtures before timing, verify output independently, retain
raw samples and alternate order. `black_box` is a best-effort optimization barrier,
not a timing guarantee. CI timings are not controlled end-to-end evidence.
Test-only nightly tooling is separately pinned to nightly-2026-09-18. Miri checks
scalar/fallback paths; it does not execute this build's AVX2 module. AddressSanitizer
instruments native kernels, not PostgreSQL and not a rebuilt standard library.
PostgreSQL 18.6 documents the per-transaction log duration in microseconds and,
for rate-controlled runs, a separate schedule-lag field. The committed parser
combines worker logs, rejects malformed records, preserves failure classes and
uses deterministic nearest-rank percentiles. The benchmark harness records raw
logs and environment data so summaries can be recomputed independently.

Tool availability and successful execution are evidence only when observed in CI.
