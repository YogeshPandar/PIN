# G0 acceptance ledger

## Selected inputs

- PostgreSQL 18.6: `724edf9bde9d356724ad384a2e196edc3c9f80f7`.
- pgrx 0.19.2: `70383e884582d1bcc7cd681d10886b995a2830cb`.
- Rust 1.98.1; exact toolchain in `rust-toolchain.toml`.
- `pgrx`, `pgrx-pg-sys`, and `cargo-pgrx` must all resolve to 0.19.2.

## Status

Implementation in progress. No Rust or PostgreSQL test has been executed in the
development VM. CI evidence, ABI inspection, and independent unsafe review are
required before G0 can be marked complete.

## Blocking proofs beyond the registration spike

1. Owner-leaf pins must interlock with synchronous heap fetch and VACUUM cleanup.
2. VACUUM must remove incarnations from retired reader-reachable sources.
3. Statement statistics epochs must bind correctly across scalar and custom plans.
4. Multi-record publication must preserve complete searchable coverage after crash.
5. Shared coordination must clean up on ERROR, abort, and backend death.
6. Standby readers need their own replay/retention protocol; primary pins do not suffice.

No production path may enable an optimization merely because a model passes.
