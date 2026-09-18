// host-independent identities, checked formats and deterministic reference search.
// only rust-owned or borrowed data; no postgresql pointers cross this crate.
// contracts: docs/api-evidence.md and docs/g1-api-evidence.md.
#![forbid(unsafe_code)]

pub mod analysis;
pub mod budget;
pub mod codec;
pub mod error;
pub mod identity;
pub mod index;
mod memory;
mod normalize;
pub mod oracle;
pub mod query;
pub mod rank;

/// The PostgreSQL major version targeted by the G0 boundary.
pub const POSTGRES_MAJOR: u32 = 18;

/// The PostgreSQL page size accepted by the G0 boundary.
pub const PAGE_BYTES: usize = 8192;
