//! Host-independent identities and resource contracts. No PostgreSQL pointers.
//! Source contracts are recorded in `docs/api-evidence.md`.
#![forbid(unsafe_code)]

/// The PostgreSQL major version targeted by the G0 boundary.
pub const POSTGRES_MAJOR: u32 = 18;

/// The PostgreSQL page size accepted by the G0 boundary.
pub const PAGE_BYTES: usize = 8192;
