# G1 API evidence

This is the isolated pure-engine supplement to `api-evidence.md`. It does not
change G0 host-boundary evidence or review status.

Rust contracts were verified against Rust 1.98.1 source revision
`48a229cea` from 2026-09-01. Unicode semantics are frozen to Unicode 16.0.0.
The accepted G1 code evidence below is for
`2f44da89802885922e0400fd7a1d38660341a8b5`.

| Entry | Official contract | Local obligation | Evidence |
|---|---|---|---|
| G1-BYTES | [u32](https://doc.rust-lang.org/std/primitive.u32.html), [slice](https://doc.rust-lang.org/std/primitive.slice.html) | Check lengths and arithmetic before slicing; serialize explicit little-endian fields, never Rust layout | `g1_codecs.rs`: golden bytes, truncation, overflow, canonical varints |
| G1-TEXT | [`str::from_utf8`](https://doc.rust-lang.org/std/str/fn.from_utf8.html), [`str`](https://doc.rust-lang.org/std/primitive.str.html) | Validate UTF-8 before exposing `&str`; use byte-lexicographic Rust string ordering consistently for dictionaries | Dictionary/value tests and prefix oracle |
| G1-ALLOC | [`Vec`](https://doc.rust-lang.org/std/vec/struct.Vec.html) | Use fallible reservation for user-controlled growth; account actual capacity; never adopt PostgreSQL allocations | Resource-limit tests and bounded analyzer/query/index scratch |
| G1-ITER | [`FusedIterator`](https://doc.rust-lang.org/std/iter/trait.FusedIterator.html), [`u64::trailing_zeros`](https://doc.rust-lang.org/std/primitive.u64.html#method.trailing_zeros) | Stay exhausted after `None`; call trailing-zero logic only for nonzero words | Position/container iterator tests and malformed-input fuzzing |
| G1-SCORE | [`f64`](https://doc.rust-lang.org/std/primitive.f64.html), [`BinaryHeap`](https://doc.rust-lang.org/std/collections/struct.BinaryHeap.html) | Require finite scoring inputs/results, fixed accumulation order and immutable heap ordering keys | Exact BM25 formula oracle, ties, eligibility and extreme arithmetic fixtures |
| G1-NORM | [UAX #15](https://www.unicode.org/reports/tr15/), [`unicode-normalization` v0.1.24](https://github.com/unicode-rs/unicode-normalization/tree/v0.1.24) | NFC, simple default case fold, NFC; reject incompatible Unicode data versions; bound normalization scratch | Unicode 16 normalization conformance and every-scalar identity/default tests |
| G1-WORD | [UAX #29](https://www.unicode.org/reports/tr29/), [`unicode-segmentation` v1.12.0](https://github.com/unicode-rs/unicode-segmentation/tree/v1.12.0) | Word boundaries and word filtering must be identical for sequential and indexed paths | Unicode 16 word-boundary vectors and ASCII/general-path equivalence |
| G1-FUZZ | [Rust Fuzz Book cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html), [CI guidance](https://rust-fuzz.github.io/book/cargo-fuzz/ci.html) | Use nightly/libFuzzer only in isolated fuzz tooling; pin toolchain/tool version; preserve seeds and artifacts; do not make production depend on nightly | `query`, `codecs` and `analysis` each completed 100,000 deterministic runs |

No new unsafe code or PostgreSQL API is used by G1. The codecs are independently
implemented from the specified bytes and standard-library contracts. Slice
`get`, checked arithmetic and explicit limits protect untrusted offset/count
fields. Validation is allocation-free where the codec contract permits it.

## Accepted CI evidence

The G1 pure-engine workflow on
`2f44da89802885922e0400fd7a1d38660341a8b5` passed `cargo check`, debug tests,
release tests, mandatory Unicode conformance tests, Clippy with `-D warnings`,
rustdoc with warnings denied, lockfile stability and Rust 1.98.1 formatting.
The ordinary suites contain 41 passing tests per debug/release run. Three
additional mandatory Unicode tests passed in the release conformance run:
19,965 NFC vectors plus 1,094,979 unlisted scalar identity cases, 1,826 word
boundary vectors, and 1,484 simple-fold mappings with default-scalar coverage.

The G1 fuzz workflow on the same commit passed 100,000 runs for each of the
`query`, `codecs` and `analysis` targets with fixed seeds. Its `fuzz/Cargo.lock`
and formatter deltas were empty. These runs establish the G1 acceptance evidence
only; they are not a performance benchmark or an absence-of-bugs guarantee.

The steady-state G1 validation workflow has read-only repository contents
permission and does not push source changes. A temporary formatter checkpoint
was used during review, then removed before the accepted runs above.
