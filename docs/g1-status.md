# G1: pure semantics and representation

Branch base: `e3b113a836d7ce09c805919e8d055970eebf1689` (`main`).

This branch is isolated from `pin-pg`, AM callbacks, ABI probes, G0 lifecycle
models/tests, `g0.yml`, and the G0 unsafe audit. It adds pure-engine work and its
own evidence. The root toolchain and existing public identity API remain
unchanged. Rebase this branch after the G0 follow-up merges.

## Implemented checkpoints

1. Checked little-endian readers/writers and canonical u32 varints.
2. Exact counted delta-position streams and independent golden-byte tests.
3. Document/value/dictionary/manifest codecs and adaptive offset containers.
4. Frozen Unicode 16 analysis, bounded typed query parser and binary query form.
5. Independent sequential document oracle and deterministic reference index.
6. Exact Boolean, prefix and repeated-term phrase evaluation.
7. Physical-corpus statistics epochs, finite BM25 and deterministic eligible-only top-k.
8. Explicit query, analysis, indexing, expansion, search-work and memory limits.
9. Deterministic Unicode conformance, differential fixtures and malformed-input fuzzing.

The disk bytes remain experimental. G1 does not implement PostgreSQL visibility,
durability, WAL, VACUUM integration or AM callbacks. No PostgreSQL pointer enters
the pure engine and `pin-core` forbids unsafe code.

## Acceptance evidence

Accepted code commit: `2f44da89802885922e0400fd7a1d38660341a8b5`.

`G1 pure engine` run 18 passed:

- `cargo check -p pin-core`.
- 41 ordinary tests in the debug suite with no failures.
- The same 41 ordinary tests in the release suite with no failures.
- Three mandatory Unicode 16 conformance tests in release mode.
- NFC coverage: 19,965 official vectors and 1,094,979 unlisted scalar identities.
- Word-boundary coverage: 1,826 official vectors.
- Simple-fold coverage: 1,484 mappings plus all scalar defaults.
- `cargo clippy --locked -p pin-core --all-targets -- -D warnings`.
- `cargo doc --locked -p pin-core --no-deps` with warnings denied.
- Zero `Cargo.lock` delta and zero Rust 1.98.1 formatter delta.

`G1 fuzz` run 12 passed:

- `query`: 100,000 deterministic libFuzzer runs.
- `codecs`: 100,000 deterministic libFuzzer runs.
- `analysis`: 100,000 deterministic libFuzzer runs.
- Zero `fuzz/Cargo.lock` delta and zero fuzz-target formatter delta.

The review-artifact workflow also passed for the accepted code head. These are
correctness and acceptance results, not throughput or latency measurements.
No performance-parity claim is made.

## Coordination state

The latest checked `main` remains
`e3b113a836d7ce09c805919e8d055970eebf1689`, so the requested G0 follow-up rebase
is not yet applicable. Keep PR #4 draft until the G0 follow-up is merged and the
G1 branch is rebased and revalidated against that new base.
