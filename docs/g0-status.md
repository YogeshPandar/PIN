# G0 acceptance ledger

## Selected inputs

- PostgreSQL 18.6: `724edf9bde9d356724ad384a2e196edc3c9f80f7`.
- pgrx 0.19.2: `70383e884582d1bcc7cd681d10886b995a2830cb`.
- Rust 1.98.1; exact toolchain in `rust-toolchain.toml`.
- Matching pgrx/pgrx-pg-sys/cargo-pgrx 0.19.2; existing Cargo lock preserved.

## Verified merged baseline

The base is `32b77d31a1e5bd49712d8257f1868bd7b263bb6c`, PR #1 merged on
17 September 2026. It is not the larger earlier local archive.

[Baseline CI run 35252527766](https://github.com/YogeshPandar/PIN/actions/runs/35252527766)
compiled and installed the small original extension against the selected server.
The pure job's five unit tests, Clippy, and rustdoc passed. Formatting failed, and
one of four protocol-model tests failed because it required more than 50 states
while the model actually reached 37. Neither job ran SQL lifecycle tests.
These results apply only to that merged commit, not to this follow-up.

## Follow-up implementation

The model now compares the exact independently generated state set and per-action
transition counts. It verifies replayable negative controls and avoids per-edge
path-vector cloning. The host crate now registers an explicit fail-closed AM,
checks all 51 C/Rust offsets, validates server presets, requires preloading, and
checks UTF-8 only in backend entry points. Runtime tests cover installation,
drop/recreation, restart, rejected index/opclass creation, repeated late loads,
non-UTF-8 installation, test-hook isolation, and guarded error cleanup.

Python model/source tests and shell syntax checks run in the development VM.
Rust/C compilation, rustfmt, Clippy, SQL execution, and independent unsafe review
remain required on this branch. Authored tests and static inventory checks are
not recorded as executed Rust/PostgreSQL tests. No new performance result exists.

G0 remains open. Host-level proof prototypes and independent review have not
been replaced by the abstract model or the registration spike.

## Blocking proofs beyond the registration spike

1. Owner-leaf pins must interlock with synchronous heap fetch and VACUUM cleanup.
2. VACUUM must remove incarnations from retired reader-reachable sources.
3. Statement statistics epochs must bind correctly across scalar and custom plans.
4. Multi-record publication must preserve complete searchable coverage after crash.
5. Shared coordination must clean up on ERROR, abort, and backend death.
6. Standby readers need their own replay/retention protocol; primary pins do not suffice.

No production path may enable an optimization merely because a model passes.
See the [unsafe register](unsafe-audit.md) for still-unassigned independent reviews.
