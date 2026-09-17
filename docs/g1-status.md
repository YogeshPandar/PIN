# G1: pure semantics and representation

Branch base: `e3b113a836d7ce09c805919e8d055970eebf1689` (`main`).

This branch is isolated from `pin-pg`, AM callbacks, ABI probes, G0 lifecycle
models/tests, `g0.yml`, and the G0 unsafe audit. It adds only pure engine work
and its own evidence. The root toolchain and existing public identity API stay
unchanged. Rebase this branch after the G0 follow-up merges; never merge G1
before reviewing its pure-Rust test evidence.

## Checkpoints

1. Checked little-endian readers/writers and canonical u32 varints.
2. Exact counted delta-position streams and independent golden-byte tests.
3. Document/value/dictionary/manifest codecs and adaptive offset containers.
4. Versioned analysis/query semantics, independent document oracle and index.
5. Frozen physical-corpus BM25 and deterministic eligible-only top-k.

Only checkpoints present in code are implemented. This is an experimental
format, not a PostgreSQL index, durability implementation, or performance claim.
Rust is not installed in the development VM; compilation and Rust tests must run
in Actions. Test code existing in a commit is not evidence of a passing run.
