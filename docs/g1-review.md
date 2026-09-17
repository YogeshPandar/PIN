# G1 completion review

Base under review: `1e52bf7c4aac74a5e17caaf113eccf168add98b8`.

Continue PR #4 without replacing its existing Unicode analyzer, flat typed
parser, or sequential oracle with a second semantic interface. Port the saved
reference-index and ranking work to those interfaces.

## Acceptance work

- Apply the Cargo-generated lockfile and reviewed formatting from Actions.
- Correct the absent-prefix fixture and use logarithmic prefix bounds.
- Complete bounded reference postings, exact phrases, physical statistics epochs,
  finite BM25 and eligible-only deterministic top-k.
- Review normalization scratch, parser edge cases, identity validation, decoder
  canonicality, work limits and failure atomicity.
- Run Unicode conformance, deterministic differential tests, malformed-input
  fuzzing, formatting, Clippy and rustdoc in Actions.

No Rust toolchain is installed or executed in the development VM. Evidence must
identify the actual tested commit. A passing old run does not validate new code.
No benchmark claim or absence-of-bugs guarantee is made.

## Isolation

Do not change `pin-pg`, AM callbacks, ABI probes/build scripts, G0 lifecycle/error
tests, `.github/workflows/g0.yml`, or G0 unsafe-boundary documentation. The
experimental formats do not implement PostgreSQL visibility or durability.

The latest checked `main` is `e3b113a836d7ce09c805919e8d055970eebf1689`.
Recheck it before handoff and integrate a merged G0 follow-up without overwriting
that work.
