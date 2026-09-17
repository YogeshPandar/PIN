//! fail-closed am registration. no callback reads or writes relation storage.
//! exact pg18.6 signatures: access/amapi.h; manuals: index-api and index-functions.
//! am01 in docs/api-evidence.md covers allocation, null callbacks, and unwind guards.

#![allow(
    clippy::too_many_arguments,
    reason = "PostgreSQL fixes the callback signatures"
)]

use pgrx::{Internal, pg_extern, pg_guard, pg_sys};
use std::ffi::c_void;

#[pg_extern(sql = r#"
CREATE FUNCTION pin.pin_handler(internal)
RETURNS index_am_handler
LANGUAGE c VOLATILE PARALLEL UNSAFE CALLED ON NULL INPUT
AS '@MODULE_PATHNAME@', '@FUNCTION_NAME@';
"#)]
pub(crate) fn pin_handler() -> Internal {
    // core passes no actual arguments to the sql handler.
    crate::compatibility::database();
    let routine = pg_sys::IndexAmRoutine {
        type_: pg_sys::NodeTag::T_IndexAmRoutine,
        amstrategies: 1,
        amsupport: 0,
        amoptsprocnum: 0,
        amcanorder: false,
        amcanorderbyop: false,
        amcanhash: false,
        amconsistentequality: false,
        amconsistentordering: false,
        amcanbackward: false,
        amcanunique: false,
        amcanmulticol: false,
        amoptionalkey: false,
        amsearcharray: false,
        amsearchnulls: false,
        amstorage: false,
        amclusterable: false,
        ampredlocks: false,
        amcanparallel: false,
        amcanbuildparallel: false,
        amcaninclude: false,
        amusemaintenanceworkmem: false,
        amsummarizing: false,
        amparallelvacuumoptions: 0,
        amkeytype: pg_sys::InvalidOid,
        ambuild: Some(build),
        ambuildempty: Some(build_empty),
        aminsert: Some(insert),
        aminsertcleanup: None,
        ambulkdelete: Some(bulk_delete),
        amvacuumcleanup: Some(vacuum_cleanup),
        amcanreturn: None,
        amcostestimate: Some(cost_estimate),
        amgettreeheight: None,
        amoptions: Some(options),
        amproperty: None,
        ambuildphasename: None,
        amvalidate: Some(validate_opclass),
        amadjustmembers: Some(adjust_members),
        ambeginscan: Some(begin_scan),
        amrescan: Some(rescan),
        amgettuple: None,
        amgetbitmap: None,
        amendscan: Some(end_scan),
        ammarkpos: None,
        amrestrpos: None,
        amestimateparallelscan: None,
        aminitparallelscan: None,
        amparallelrescan: None,
        amtranslatestrategy: None,
        amtranslatecmptype: None,
    };
    // safety: palloc returns context-owned storage aligned for this checked abi.
    let node = unsafe { pg_sys::palloc(std::mem::size_of::<pg_sys::IndexAmRoutine>()) }
        .cast::<pg_sys::IndexAmRoutine>();
    // safety: this fresh allocation holds the full value and has no aliases.
    unsafe { node.write(routine) };
    Internal::from(Some(pg_sys::Datum::from(node)))
}

pub(crate) fn unavailable() -> ! {
    pgrx::ereport!(
        ERROR,
        pgrx::PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
        "Pin G0 does not implement index storage or search"
    );
}

#[pg_guard]
unsafe extern "C-unwind" fn build(
    _heap: pg_sys::Relation,
    _index: pg_sys::Relation,
    _info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn build_empty(_index: pg_sys::Relation) {
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn insert(
    _index: pg_sys::Relation,
    _values: *mut pg_sys::Datum,
    _nulls: *mut bool,
    _tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _unique: pg_sys::IndexUniqueCheck::Type,
    _unchanged: bool,
    _info: *mut pg_sys::IndexInfo,
) -> bool {
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn bulk_delete(
    _info: *mut pg_sys::IndexVacuumInfo,
    _stats: *mut pg_sys::IndexBulkDeleteResult,
    _callback: pg_sys::IndexBulkDeleteCallback,
    _state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn vacuum_cleanup(
    _info: *mut pg_sys::IndexVacuumInfo,
    _stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn cost_estimate(
    _root: *mut pg_sys::PlannerInfo,
    _path: *mut pg_sys::IndexPath,
    _loops: f64,
    _startup: *mut pg_sys::Cost,
    _total: *mut pg_sys::Cost,
    _selectivity: *mut pg_sys::Selectivity,
    _correlation: *mut f64,
    _pages: *mut f64,
) {
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn options(_options: pg_sys::Datum, validate: bool) -> *mut pg_sys::bytea {
    // validate new options; ignore unsupported options during catalog loading.
    if validate {
        unavailable();
    }
    std::ptr::null_mut()
}

#[pg_guard]
unsafe extern "C-unwind" fn validate_opclass(_oid: pg_sys::Oid) -> bool {
    false
}

#[pg_guard]
unsafe extern "C-unwind" fn adjust_members(
    _family: pg_sys::Oid,
    _class: pg_sys::Oid,
    _operators: *mut pg_sys::List,
    _functions: *mut pg_sys::List,
) {
    #[cfg(feature = "test-hooks")]
    let _probe = crate::test_hooks::DropProbe;
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn begin_scan(
    _index: pg_sys::Relation,
    _keys: i32,
    _orderbys: i32,
) -> pg_sys::IndexScanDesc {
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn rescan(
    _scan: pg_sys::IndexScanDesc,
    _keys: pg_sys::ScanKey,
    _nkeys: i32,
    _orderbys: pg_sys::ScanKey,
    _norderbys: i32,
) {
    unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn end_scan(_scan: pg_sys::IndexScanDesc) {
    // begin_scan cannot create resources in g0.
}
