# G1 completion review

Accepted code head: `2f44da89802885922e0400fd7a1d38660341a8b5`.
Branch base: `e3b113a836d7ce09c805919e8d055970eebf1689`.

PR #4 preserves the existing Unicode analyzer, flat typed parser and sequential
oracle and integrates the reference index and ranking engine with those semantic
interfaces rather than adding a competing API.

## Acceptance review

Completed:

- Applied the generated lockfile and Rust 1.98.1 formatting required by CI.
- Corrected absent-prefix behavior and retained logarithmic dictionary bounds.
- Implemented bounded reference postings, exact phrases and explicit NOT universe semantics.
- Implemented coherent physical statistics epochs, finite BM25 and eligible-only deterministic top-k.
- Reviewed normalization scratch, parser limits, identity validation, decoder canonicality, work limits and error behavior.
- Added independent Boolean/phrase/index/ranking oracles and mixed-container equivalence tests.
- Added Unicode 16 normalization, word-boundary and simple-fold conformance gates.
- Added deterministic libFuzzer targets for queries, codecs and analysis.
- Verified debug/release tests, Clippy, rustdoc, formatting and lockfile stability in Actions.

The accepted pure-engine run has 41 ordinary passing tests in both debug and
release modes plus three mandatory Unicode conformance tests. The fuzz run has
300,000 total deterministic executions across the three targets. Exact details
and official contracts are recorded in `g1-status.md` and `g1-api-evidence.md`.

No Rust toolchain was installed or executed in the development VM. No benchmark
claim or absence-of-bugs guarantee is made.

## Isolation

The PR does not change `pin-pg`, AM callbacks, ABI probes/build scripts, G0
lifecycle/error tests, `.github/workflows/g0.yml`, or G0 unsafe-boundary
documentation. The experimental formats do not implement PostgreSQL visibility
or durability.

## Remaining coordination

The latest checked `main` is still
`e3b113a836d7ce09c805919e8d055970eebf1689`. The G1 technical gate is accepted on
the code head above, but PR #4 remains draft until the G0 follow-up is merged.
At that point rebase G1 onto the updated `main`, resolve only genuine conflicts,
and rerun the G1 pure/fuzz gates before handoff.
