# G8 operational qualification

Base: `69c9ad745eec62b5e33188381466e20e2952c1db`.
Status: qualification candidate, not a production release.

G8 follows blueprint sections 21 through 25. It adds executable operational
checks and deployment tooling; it does not promote earlier experimental features
or claim PostgreSQL/Tin performance parity. No tag or public release is created.

## Implemented scope

The normal deployment build records its source revision and rejects test-hook
features. The package assembler preserves generated SQL, control and library
files, compatibility/runbook documentation, lockfile, feature graph and actual
dependency notices. It verifies a fixed bounded archive inventory without
extracting archive paths. Repeated assembly tests archive reproducibility, not
cross-machine shared-library reproducibility or publisher authenticity.

Native TAP suites exercise normal installation, physical backup verification,
restore, named-target PITR, streaming replay, promotion, post-promotion writes,
logical dump/restore, subscriber-local index maintenance, supported DDL,
privilege/RLS behavior and explicit unsupported-operation diagnostics. They
check actual plans and compare indexed versus sequential row identities.

See [operations](operations.md), [compatibility](compatibility.md),
[security self-review](g8-security-review.md) and [source evidence](g8-api-evidence.md).

## Validation record

Local Python unit tests and source-contract checks pass. Rust is not installed
in the editing VM; compilation and native execution use the existing CI.

At checkpoint `00432299a83ac7c8e67fef64b76da09f8f898d25`, the pure Rust tests,
formatting, Clippy and rustdoc jobs passed. The extension compiled and normal
installation/lifecycle smoke tests passed. G8's first 19 TAP assertions passed,
including normal build identity, gated defaults, exact bitmap results and online
base backup. The suite then stopped because its prerequisite incorrectly used
`command_ok` as a Boolean. The helper returns no value; the corrected test uses
`run_log` and records the actual result. Later native checks were not executed
in that run. Subsequent candidate commits must attach their own CI evidence.

## Remaining release gates

The target remains PostgreSQL 18.6, Rust 1.98.1, pgrx 0.19.2, x86_64 GNU/Linux,
UTF-8, upstream heap and 8192-byte pages. SQL version `0.0.0` is not a published
upgrade baseline. Experimental snapshots require an explicit compatibility
review and rebuild; no fictional no-op update script is supplied.

Hot-standby Pin storage reads and SQL ranked execution remain unsupported.
Count/VM, parallel maintenance and retained-prefix compaction remain gated.
Other PostgreSQL versions, architectures, pg_upgrade, distribution packages and
unadvertised DDL variants are not qualified by this PR.

Independent storage/FFI/security review, a functioning private reporting route,
full passing operational CI, sustained resource measurements, controlled
benchmarks and attribution review remain release blockers. Every claimed result
must identify the candidate commit. Checksums and self-review do not replace
those requirements; the repository owner retains the release decision.
