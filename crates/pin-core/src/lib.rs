//! Host-independent identities and checked resource accounting.
//! Values own only integers; no PostgreSQL pointer or allocator crosses this crate.
//! Contracts: `docs/api-evidence.md`, entries ID01 and RS01.
#![forbid(unsafe_code)]

pub mod budget;
pub mod identity;

/// The PostgreSQL major version targeted by the G0 boundary.
pub const POSTGRES_MAJOR: u32 = 18;

/// The PostgreSQL page size accepted by the G0 boundary.
pub const PAGE_BYTES: usize = 8192;
