# Implementation rules

Read the exact official PostgreSQL, pgrx, Rust, and Cargo contracts before writing
or changing a material boundary, and recheck them during review. Record the
contract, immutable upstream source, local obligations, tests, and review status
in `docs/api-evidence.md`. Do not guess bindings or copy another extension's
unsafe assumptions. Keep unresolved correctness questions as explicit gates.

Preserve the merged branch and its Cargo-generated lockfile. Work on a new
feature branch; commit focused checkpoints and push when access permits. Never
force-push the user's branches. No Rust installation is performed in the
development VM. Use CI for the selected Rust and PostgreSQL test matrix.

Keep comments short, specific, and lowercase. Local `safety:` comments must state
the proof at the call site. Use safe Rust by default, checked arithmetic, explicit
ownership, and bounded work. New unsafe operations require independent review.
Do not optimize a hot path without an oracle and measured evidence. Do not claim
benchmarks, test success, remote commits, or a PR without observing them.

SQL functions must preserve PostgreSQL error, transaction, and security rules.
Unsupported storage and visibility features must remain disabled. No PostgreSQL
pointer enters the pure engine. Document actual API usage and limitations with
examples; do not use emojis or em dashes.
