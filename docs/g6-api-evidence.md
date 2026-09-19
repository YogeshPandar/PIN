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
- https://doc.rust-lang.org/std/primitive.slice.html#method.chunks_exact_mut
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
Actual compiler/test results and applicable sanitizer/Miri execution must be
recorded after CI. These tests do not prove PostgreSQL visibility or performance.

## Performance gate

No production call site selects automatic SIMD yet. Kernel benchmarks are
experiments; controlled PostgreSQL benefit at equal durability/memory and
independent unsafe review are still required before default activation.
