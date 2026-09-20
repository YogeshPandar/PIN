//! durable logged-table bitmap integration; PostgreSQL retains MVCC authority.
//! exact pg18.6 signatures: access/amapi.h; manuals: index-api and index-functions.
//! am01 in docs/api-evidence.md covers allocation, null callbacks, and unwind guards.

#![allow(
    clippy::too_many_arguments,
    reason = "PostgreSQL fixes the callback signatures"
)]

use crate::{matching, native, storage};
use pgrx::{FromDatum, Internal, pg_extern, pg_guard, pg_sys};
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::candidate::CandidatePlan;
use pin_core::error::Error;
use pin_core::mutable::{self, document::PreparedDocument};
use pin_core::query::{Query, QueryLimits};
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
        aminsertcleanup: Some(insert_cleanup),
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
        amgetbitmap: Some(bitmap),
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
        "Pin does not support this operation in the bitmap baseline"
    );
}

// core owns every relation, datum, callback and descriptor passed to these entries.
#[pg_guard]
unsafe extern "C-unwind" fn build(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    crate::compatibility::database();
    // safety: core supplies live locked relations and IndexInfo for this build.
    unsafe { native::call(|| native::pin_storage_check(index, heap, info)) };
    // safety: the relation stays open through the synchronous initialization.
    matching::stored(unsafe { storage::with_writer(index, |store| mutable::initialize(store)) });
    let mut state = BuildState { heap, documents: 0 };
    let state_ptr = std::ptr::from_mut(&mut state).cast::<c_void>();
    // safety: the core scan maps HOT roots and evaluates index expressions/predicates.
    // state lives through all sequential, guarded callback invocations.
    let heap_tuples = unsafe {
        native::call(|| {
            native::pin_heap_build_scan(heap, index, info, Some(build_tuple), state_ptr)
        })
    };
    // safety: palloc returns aligned PostgreSQL-owned memory; both fields are initialized.
    unsafe {
        let result = pg_sys::palloc(std::mem::size_of::<pg_sys::IndexBuildResult>())
            .cast::<pg_sys::IndexBuildResult>();
        result.write(pg_sys::IndexBuildResult {
            heap_tuples,
            index_tuples: state.documents as f64,
        });
        result
    }
}

struct BuildState {
    heap: pg_sys::Relation,
    documents: u64,
}

#[pg_guard]
unsafe extern "C-unwind" fn build_tuple(
    index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    nulls: *mut bool,
    _tuple_is_alive: bool,
    state: *mut c_void,
) {
    // safety: build keeps this unique state live; callbacks run sequentially.
    let state = unsafe { &mut *state.cast::<BuildState>() };
    // safety: the core build scan supplies one evaluated text key and its HOT root.
    if unsafe { insert_value(index, state.heap, values, nulls, tid) } {
        state.documents = matching::stored(
            state
                .documents
                .checked_add(1)
                .ok_or(Error::Limit("index document count")),
        );
    }
}

/// # Safety
/// all pointers are core callback inputs for the validated one-text-key index.
unsafe fn insert_value(
    index: pg_sys::Relation,
    _heap: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    nulls: *mut bool,
    tid: pg_sys::ItemPointer,
) -> bool {
    storage::interrupt();
    // safety: core supplies initialized one-element key and null arrays.
    if unsafe { *nulls } {
        return false;
    }
    // safety: the opclass enforces text; pgrx detoasts it in the current context.
    // the borrowed text is consumed before any context reset and never retained.
    let text = unsafe { <&str as FromDatum>::from_datum(*values, false) };
    let text = matching::input(text.ok_or(Error::InvalidDocument));
    let analyzed = matching::input(Analyzed::analyze(text, AnalysisLimits::default()));
    let document = matching::input(PreparedDocument::prepare(
        &analyzed,
        matching::PREPARE_MEMORY,
    ));
    drop(analyzed);
    // safety: C reads a valid core-owned item pointer without changing its root identity.
    let root = matching::stored(unsafe { storage::root(tid) });
    // safety: analysis/allocation precede the writer lock; relation lifetime is unchanged.
    matching::stored(unsafe {
        storage::with_writer(index, |store| mutable::insert(store, root, &document))
    });
    true
}

#[pg_guard]
unsafe extern "C-unwind" fn build_empty(_index: pg_sys::Relation) {
    // unlogged tables are rejected before a successful main-fork build.
    unavailable();
}

#[pg_guard]
unsafe extern "C-unwind" fn insert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    nulls: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    heap: pg_sys::Relation,
    uniqueness: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    info: *mut pg_sys::IndexInfo,
) -> bool {
    crate::compatibility::database();
    if uniqueness != pg_sys::IndexUniqueCheck::UNIQUE_CHECK_NO {
        unavailable();
    }
    // safety: core retains both relations and IndexInfo; C checks persistence/layout.
    unsafe { native::call(|| native::pin_storage_check(index, heap, info)) };
    // safety: evaluated key arrays and the root belong to this guarded insertion.
    unsafe { insert_value(index, heap, values, nulls, heap_tid) };
    false
}

#[pg_guard]
unsafe extern "C-unwind" fn insert_cleanup(
    _index: pg_sys::Relation,
    _info: *mut pg_sys::IndexInfo,
) {
    // every insertion is already published; no statement cache or retained pins exist.
}

#[pg_guard]
unsafe extern "C-unwind" fn bulk_delete(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    callback: pg_sys::IndexBulkDeleteCallback,
    state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    if callback.is_none() {
        unavailable();
    }
    // safety: core supplies a live vacuum descriptor through the entire callback.
    unsafe { vacuum_pass(info, stats, callback, state) }
}

#[pg_guard]
unsafe extern "C-unwind" fn vacuum_cleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    // safety: core initializes info; ANALYZE must not perform index reclamation.
    if unsafe { (*info).analyze_only } {
        return stats;
    }
    // safety: when bulk delete was skipped, collect live/free statistics first.
    let stats = if stats.is_null() {
        // safety: the live VACUUM descriptor remains valid through the cleanup pass.
        unsafe { vacuum_pass(info, stats, None, std::ptr::null_mut()) }
    } else {
        stats
    };
    // safety: final cleanup runs once after all bulk-delete cycles.
    unsafe { compact_cleanup(info, stats) }
}

/// # Safety
/// info, stats and callback/state are the live PostgreSQL VACUUM parameters.
unsafe fn vacuum_pass(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    callback: pg_sys::IndexBulkDeleteCallback,
    state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    crate::compatibility::database();
    // safety: descriptor fields stay valid while core holds the VACUUM relation locks.
    let (index, heap) = unsafe { ((*info).index, (*info).heaprel) };
    // safety: only live relation pointers enter the checked C compatibility gate.
    unsafe { native::call(|| native::pin_storage_check(index, heap, std::ptr::null_mut())) };
    // safety: the writer interlock serializes owner liveness and free-list changes.
    let result = matching::stored(unsafe {
        storage::with_writer(index, |store| {
            mutable::vacuum(store, |root| {
                if callback.is_none() {
                    return Ok(false);
                }
                let block = root.block();
                let offset = root.offset();
                Ok(native::call(|| {
                    native::pin_vacuum_removable(callback, state, block, offset)
                }))
            })
        })
    });
    // safety: PostgreSQL owns this initialized statistics record across VACUUM rounds.
    unsafe {
        let stats = if stats.is_null() {
            pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
                .cast::<pg_sys::IndexBulkDeleteResult>()
        } else {
            stats
        };
        (*stats).num_pages = result.pages;
        (*stats).estimated_count = false;
        (*stats).num_index_tuples = result.live_documents as f64;
        (*stats).tuples_removed += result.removed_documents as f64;
        (*stats).pages_newly_deleted = matching::stored(
            (*stats)
                .pages_newly_deleted
                .checked_add(result.reclaimed_pages)
                .ok_or(Error::Limit("VACUUM pages")),
        );
        (*stats).pages_deleted = result.free_pages;
        (*stats).pages_free = result.free_pages;
        stats
    }
}

/// # Safety
/// info and stats are live PostgreSQL VACUUM cleanup parameters.
unsafe fn compact_cleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    crate::compatibility::database();
    // safety: descriptor fields stay valid while core holds the VACUUM relation locks.
    let (index, heap) = unsafe { ((*info).index, (*info).heaprel) };
    // safety: only live relation pointers enter the checked C compatibility gate.
    unsafe { native::call(|| native::pin_storage_check(index, heap, std::ptr::null_mut())) };
    // safety: physical posting reclamation waits for readers, then excludes writers.
    let (compacted, pages) = matching::stored(unsafe {
        storage::with_maintenance(index, |store| {
            let compacted = mutable::compact_with_mode(store, crate::maintenance::mode())?;
            let pages = pin_core::mutable::PageStore::blocks(store)?;
            Ok((compacted, pages))
        })
    });
    crate::maintenance::report(compacted);
    // safety: vacuum_pass always returns a live PostgreSQL-owned statistics record.
    unsafe {
        let free_pages = matching::stored(
            (*stats)
                .pages_free
                .checked_add(compacted.reclaimed_pages)
                .and_then(|pages| pages.checked_sub(compacted.reused_pages))
                .ok_or(Error::InvalidState),
        );
        (*stats).num_pages = pages;
        (*stats).pages_newly_deleted = matching::stored(
            (*stats)
                .pages_newly_deleted
                .checked_add(compacted.reclaimed_pages)
                .ok_or(Error::Limit("VACUUM pages")),
        );
        (*stats).pages_deleted = free_pages;
        (*stats).pages_free = free_pages;
        stats
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn cost_estimate(
    root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    loops: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    // safety: the planner provides live inputs and five distinct writable outputs.
    unsafe {
        native::call(|| {
            native::pin_index_cost(
                root,
                path,
                loops,
                startup,
                total,
                selectivity,
                correlation,
                pages,
            )
        })
    };
}

#[pg_guard]
unsafe extern "C-unwind" fn options(_options: pg_sys::Datum, validate: bool) -> *mut pg_sys::bytea {
    if !validate {
        return std::ptr::null_mut();
    }
    unavailable();
}

#[pg_guard]
unsafe extern "C-unwind" fn validate_opclass(opclass: pg_sys::Oid) -> bool {
    // safety: the catalog OID is checked through PostgreSQL's syscache routines.
    unsafe { native::call(|| native::pin_opclass_validate(opclass)) }
}

#[pg_guard]
unsafe extern "C-unwind" fn adjust_members(
    _family: pg_sys::Oid,
    class: pg_sys::Oid,
    operators: *mut pg_sys::List,
    functions: *mut pg_sys::List,
) {
    #[cfg(feature = "test-hooks")]
    let _probe = crate::test_hooks::DropProbe;
    // safety: core owns both OpFamilyMember lists; C only validates their contents.
    unsafe { native::call(|| native::pin_opclass_adjust(class, operators, functions)) };
}

#[pg_guard]
unsafe extern "C-unwind" fn begin_scan(
    index: pg_sys::Relation,
    keys: i32,
    orderbys: i32,
) -> pg_sys::IndexScanDesc {
    crate::compatibility::database();
    // safety: core holds a live index relation; C allocates the core descriptor and
    // a context-owned integer capacity record, with no Rust destructor requirement.
    unsafe {
        native::call(|| {
            native::pin_storage_check(index, std::ptr::null_mut(), std::ptr::null_mut())
        });
        native::call(|| native::pin_scan_begin(index, keys, orderbys))
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn rescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    key_count: i32,
    _orderbys: pg_sys::ScanKey,
    orderby_count: i32,
) {
    // safety: core owns scan and key arrays; C bounds the copy and preserves NULL keys.
    unsafe { native::call(|| native::pin_scan_rescan(scan, keys, key_count, orderby_count)) };
}

#[pg_guard]
unsafe extern "C-unwind" fn bitmap(
    scan: pg_sys::IndexScanDesc,
    bitmap: *mut pg_sys::TIDBitmap,
) -> i64 {
    // safety: core owns the descriptor, current MVCC snapshot and key array.
    unsafe { native::call(|| native::pin_scan_validate(scan)) };
    // safety: C validated the dimensions; copy scalars rather than retain C field borrows.
    let (index, key_count, keys) =
        unsafe { ((*scan).indexRelation, (*scan).numberOfKeys, (*scan).keyData) };
    let mut chosen = None;
    let mut cost = usize::MAX;
    for position in 0..key_count as usize {
        storage::interrupt();
        // safety: position is bounded by the validated initialized scan-key array.
        let (datum, null) = unsafe {
            let key = keys.add(position);
            (
                (*key).sk_argument,
                (*key).sk_flags & pg_sys::SK_ISNULL as i32 != 0,
            )
        };
        if null {
            return 0;
        }
        // safety: the exact registered operator accepts the validated bytea-based query
        // domain; pgrx detoasts in this callback's context and no borrow escapes it.
        let bytes = unsafe { <&[u8] as FromDatum>::from_datum(datum, false) };
        let bytes = matching::input(bytes.ok_or(Error::InvalidParameters));
        let query = matching::input(Query::decode(bytes, QueryLimits::default()));
        let plan = matching::input(CandidatePlan::build(&query, matching::QUERY_MEMORY));
        let next_cost = match &plan {
            CandidatePlan::Empty => 0,
            CandidatePlan::Universe => usize::MAX,
            CandidatePlan::Terms(terms) => terms.len(),
        };
        drop(plan);
        if chosen.is_none() || next_cost < cost {
            cost = next_cost;
            chosen = Some(query);
        }
    }
    let query = matching::input(chosen.ok_or(Error::InvalidParameters));
    // safety: the caller's existing bitmap remains writable for the complete scan.
    let mut sink = unsafe { storage::BitmapSink::new(bitmap) };
    // safety: the read barrier covers all page references, not later heap visibility work.
    let count = matching::stored(unsafe {
        storage::with_reader(index, |store| {
            mutable::scan_query(store, &query, matching::QUERY_MEMORY, |root| {
                sink.push(root)
            })
        })
    });
    sink.flush();
    matching::stored(i64::try_from(count).map_err(|_| Error::Limit("bitmap accounting")))
}

#[pg_guard]
unsafe extern "C-unwind" fn end_scan(scan: pg_sys::IndexScanDesc) {
    // safety: release only our context-owned capacity record, never the core descriptor.
    unsafe { native::call(|| native::pin_scan_end(scan)) };
}
