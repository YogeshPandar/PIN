# Pin

PostgreSQL-native, Rust-first full-text search under development.

## Current scope: transactional bitmap search and experimental counts

The repository implements a logged permanent-heap search baseline: versioned
text/query semantics, durable complete-document publication, bitmap/heap scans,
canonical owner liveness, copying posting compaction, and bounded streaming
Boolean candidate execution. PostgreSQL performs snapshot visibility and exact
predicate rechecks on the ordinary bitmap path.

This G5 branch adds an experimental, default-off `PinCount` upper plan for plain
`COUNT(*)` search shapes. It streams candidate accounting without a result
bitmap, keeps mutable and uncertified candidates on HOT-aware heap visibility,
and may certify exact sealed-term candidates with fresh visibility-map status
while a canonical owner pin protects liveness. A real PostgreSQL aggregate
remains available as runtime fallback. Both `pin.enable_count_fastpath` and
`pin.enable_count_vm` default to `off` and are superuser-settable.

This is not the full blueprint's G4/G5 acceptance gate. Synchronous `amgettuple`,
SQL ranked CustomScan, general Boolean/phrase VM certification, parallel
execution and SIMD remain unsupported. G5 Rust/C compilation and PostgreSQL host
qualification must be observed in CI; there is no production-readiness or
measured performance-parity claim. See
[docs/g5-counts.md](docs/g5-counts.md) for eligibility, proof obligations and
remaining gates.

The selected target is PostgreSQL 18.6, Rust 1.98.1, matching pgrx/cargo-pgrx/
pgrx-pg-sys 0.19.2, native x86_64 GNU/Linux, UTF-8 databases, and 8 KiB pages.
Both compiled headers and running-server presets are checked. Other minor
versions require explicit review. The Cargo-generated lockfile is unchanged.

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

Configure `pin` in the server's `shared_preload_libraries`, preserving existing
entries, and restart. Use only a disposable development server while the
experimental gates remain unqualified.

```sql
CREATE EXTENSION pin;
SELECT pin.build_stage();
SELECT pin.abi_check();
SELECT pin.max_heap_offsets(), pin.generic_wal_page_limit();
```

`abi_check()` compares the selected AM field offsets, sizes, alignments and
constants. The other diagnostics expose header-derived capacities, not tuning
controls or performance claims.

## Search example

```sql
CREATE TABLE documents(id bigint PRIMARY KEY, body text);
CREATE INDEX documents_body_pin ON documents USING pin(body);
INSERT INTO documents VALUES (1, 'rust postgres'), (2, 'buffer manager');

SELECT id FROM documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('rust AND postgres');

-- Correct with both count optimizations disabled.
SELECT count(*) FROM ONLY documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('postgres');
```

The count switches are diagnostic experiments, not production defaults:

```sql
SET pin.enable_count_fastpath = on;
SET pin.enable_count_vm = on;
EXPLAIN (ANALYZE, BUFFERS)
SELECT count(*) FROM ONLY documents
WHERE body OPERATOR(pin.@@@) pin.parse_query('postgres');
```

Unsupported or changed execution conditions retain the core aggregate fallback.
Disabling either optimization must change only execution strategy, not SQL
semantics.

## Test and review

[CONTRIBUTING.md](CONTRIBUTING.md) describes the test commands.
[docs/api-evidence.md](docs/api-evidence.md) records pinned official contracts,
and [docs/unsafe-audit.md](docs/unsafe-audit.md) records review gates.

The publication model retains the existing bounded negative controls. G5 adds a
deterministic owner/VM model with six correct schedules and four deliberately
broken variants that must produce counterexamples. Abstract models do not prove
real PostgreSQL lock, WAL, planner or MVCC behavior.

The test-hooks build adds deterministic G5 pauses after owner-pin acquisition and
around visibility decisions. SQL and multi-backend schedules are wired into
`tools/g2_qualification.sh`. Only the test-hooks build contains these controls,
and PUBLIC execution is revoked. Never deploy a test-hook build to production.

The development VM does not install Rust. CI is the authority for Rust/C
compilation, rustfmt, Clippy, rustdoc and real PostgreSQL execution. Performance
remains unmeasured until a controlled benchmark gate is run.
