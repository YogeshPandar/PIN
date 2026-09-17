# Pin

PostgreSQL-native, Rust-first full-text search under development.

## Current scope: G0 host boundary

This branch registers a deliberately unusable index access method, not a searchable
index. It implements exact host checks, guarded callback entry points, independent
C/Rust ABI probes, and disposable-cluster lifecycle/error tests. No operator class,
posting storage, query parser, ranking, heap-visibility shortcut, or SIMD is enabled.
CREATE INDEX is rejected rather than creating an incomplete index. No performance
results or production-readiness claim are made.

The selected target is PostgreSQL 18.6, Rust 1.98.1, matching pgrx/cargo-pgrx/
pgrx-pg-sys 0.19.2, native x86_64 GNU/Linux, UTF-8 databases, and 8 KiB pages. Both
compiled headers and running-server presets are checked. Other minor versions
require explicit review, not an assumption of compatibility. The existing
Cargo-generated lockfile is unchanged by this follow-up.

## Build and inspect

Use the full repository checkout and an existing PostgreSQL 18.6 development
installation. The compiler is selected by `rust-toolchain.toml`.

```sh
export PGRX_PG_CONFIG_PATH=/absolute/path/to/pg18/bin/pg_config
cargo install cargo-pgrx --version 0.19.2 --locked
cargo pgrx init --pg18 "$PGRX_PG_CONFIG_PATH"
cargo build --locked -p pin-pg
(cd crates/pin-pg && cargo pgrx install --pg-config "$PGRX_PG_CONFIG_PATH")
git diff --exit-code -- Cargo.lock
```

Configure `pin` in the server's `shared_preload_libraries`, preserving any existing
entries, and restart. G0's preload requirement establishes the deployment
contract; shared coordination structures are not implemented yet. Use only a
disposable development server while this boundary is unqualified.

```sql
CREATE EXTENSION pin;
SELECT pin.build_stage();
SELECT pin.abi_check();
SELECT pin.max_heap_offsets(), pin.generic_wal_page_limit();
```

`build_stage()` retains the original diagnostic API. `abi_check()` compares the
51 AM field offsets, sizes, alignments, and selected constants and returns true
only after validation. A mismatch raises an error. The other diagnostics expose
header-derived capacities, not tuning controls or supported index functionality.
All diagnostics require a connected UTF-8 backend. `DROP EXTENSION pin` removes
catalog objects; it does not unload the shared library from existing processes.

## Test and review

[CONTRIBUTING.md](CONTRIBUTING.md) describes the test commands.
[docs/g0-status.md](docs/g0-status.md) separates baseline CI evidence from unrun
follow-up checks. [docs/api-evidence.md](docs/api-evidence.md) records official
contracts, and [docs/unsafe-audit.md](docs/unsafe-audit.md) records review gates.

The pure publication model is checked against an independently enumerated
37-state, 76-transition fixture. Broken deletion-reconciliation and retired-reader
variants must still produce replayable counterexamples. These abstract models do
not establish PostgreSQL lock, WAL, or MVCC correctness.

The host workflow installs and tests normal and `test-hooks` builds separately.
Only the latter contains fixed error probes, with PUBLIC execution revoked.
Never deploy a test-hook build to production. The normal build is checked for
their absence. The development VM does not install Rust; CI is required for
Rust/C compilation, Clippy, rustfmt, and real PostgreSQL execution.
