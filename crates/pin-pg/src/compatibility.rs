//! server presets are checked at preload; encoding is checked only in a backend.
//! borrowed guc strings never survive another host call. see compat01 in the ledger.

use pgrx::pg_sys;
use std::ffi::CStr;

pub(crate) fn server() {
    if !setting_matches(c"server_version_num", b"180006")
        || !setting_matches(c"block_size", b"8192")
    {
        pgrx::ereport!(
            ERROR,
            pgrx::PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            "Pin G0 requires a PostgreSQL 18.6 server with 8192-byte pages"
        );
    }
}

fn setting_matches(name: &CStr, expected: &[u8]) -> bool {
    // safety: preload runs after guc initialization; names are fixed, nul-terminated presets.
    let value = unsafe { pg_sys::GetConfigOption(name.as_ptr(), false, false) };
    if value.is_null() {
        return false;
    }
    // safety: postgres returns a terminated string; no host call occurs during this borrow.
    unsafe { CStr::from_ptr(value) }.to_bytes() == expected
}

pub(crate) fn database() {
    // safety: callers are guarded sql/am entries with an initialized database backend.
    let encoding = unsafe { pg_sys::GetDatabaseEncoding() };
    if encoding as u64 != crate::abi::constant(12) {
        pgrx::ereport!(
            ERROR,
            pgrx::PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            "Pin G0 requires a UTF-8 database"
        );
    }
}
