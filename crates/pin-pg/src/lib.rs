//! PostgreSQL-owned host boundary. Search and storage are not enabled in G0.
//! See `docs/api-evidence.md` for the exact pgrx and PostgreSQL contracts.
use pgrx::prelude::*;

pgrx::pg_module_magic!();

#[pg_extern]
fn build_stage() -> &'static str {
    "G0: host boundary only; indexing disabled"
}
