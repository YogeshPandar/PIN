# Contributing

Follow [AGENTS.md](AGENTS.md) for both human and automated changes. Read official
contracts before implementation and during review. A boundary change must update
the evidence ledger and unsafe register, including still-unassigned reviewers.
Do not merge a new unsafe/visibility/storage protocol on self-review alone.

## Validation

The selected host is native x86_64 GNU/Linux, PostgreSQL 18.6 with 8 KiB pages,
Rust 1.98.1, and pgrx/cargo-pgrx/pgrx-pg-sys 0.19.2. Use the committed lockfile.

```sh
python3 -m unittest discover -s tests -v
python3 tools/check_contracts.py
bash -n tools/pg_smoke.sh
cargo fmt --all --check
cargo test --locked -p pin-core
cargo clippy --locked -p pin-core --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --locked -p pin-core --no-deps
```

Rust commands run in CI when the development VM has no Rust toolchain. The
PostgreSQL workflow compiles the pinned upstream server, installs the extension,
and runs isolated test clusters in both normal and test-hook configurations.
`tools/pg_smoke.sh` refuses root and never connects to an existing server. It
requires the installed extension and `PGRX_PG_CONFIG_PATH`; logs are kept in
`.artifacts/`. Never install the test-hook build into a production cluster.

A PR must identify the affected invariant, official contracts, actual checks run,
unrun checks, compatibility changes, and remaining gates. Preserve counterexample
fixtures. Record resource work removed, but claim a speedup only with reproducible
measurements. A source-level check does not substitute for compiling or executing
Rust/PostgreSQL.
