//! postgresql-owned host boundary; storage and search remain disabled.
//! abi, ownership, and error contracts are recorded in docs/api-evidence.md.

#[cfg(not(feature = "pg18"))]
compile_error!("pin-pg requires the pg18 feature");
#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
compile_error!("G0 supports native x86_64-unknown-linux-gnu only");

use pgrx::prelude::*;

mod abi;
mod am;
mod compatibility;
#[cfg(feature = "test-hooks")]
mod test_hooks;

pgrx::pg_module_magic!();

/// validates the compiled abi and requires postmaster preloading.
///
/// # Safety
/// postgresql alone calls this entry through its library initialization protocol.
#[pg_guard]
pub unsafe extern "C-unwind" fn _PG_init() {
    compatibility::server();
    if let Err(message) = abi::validate() {
        pgrx::ereport!(
            ERROR,
            pgrx::PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            message
        );
    }
    // safety: postgres owns this startup flag and calls this on the main thread.
    if !unsafe { pg_sys::process_shared_preload_libraries_in_progress } {
        pgrx::ereport!(
            ERROR,
            pgrx::PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
            "Pin requires shared_preload_libraries = 'pin'; configure it and restart"
        );
    }
}

#[pg_extern(stable, parallel_unsafe)]
fn build_stage() -> &'static str {
    compatibility::database();
    "G0: host boundary only; indexing disabled"
}

#[pg_extern(stable, parallel_unsafe)]
fn abi_check() -> bool {
    compatibility::database();
    if let Err(message) = abi::validate() {
        pgrx::ereport!(
            ERROR,
            pgrx::PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            message
        );
    }
    true
}

#[pg_extern(stable, parallel_unsafe)]
fn max_heap_offsets() -> i32 {
    compatibility::database();
    abi::constant(9) as i32
}

#[pg_extern(stable, parallel_unsafe)]
fn generic_wal_page_limit() -> i32 {
    compatibility::database();
    abi::constant(10) as i32
}

pgrx::extension_sql!(
    "SELECT pin.abi_check(); CREATE ACCESS METHOD pin TYPE INDEX HANDLER pin.pin_handler;",
    name = "pin_access_method",
    requires = [am::pin_handler, abi_check]
);
