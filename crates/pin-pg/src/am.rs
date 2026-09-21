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
use pin_core::mutable::{
    self,
    document::PreparedDocument,
    work::{WORK_WORDS, WorkState},
};
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
        amcanparallel: true,
        amcanbuildparallel: true,
        amcaninclude: false,
        amusemaintenanceworkmem: false,
        amsummarizing: false,
        amparallelvacuumoptions: crate::parallel::vacuum_options(),
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
        amgettuple: Some(native::pin_scan_gettuple),
        amgetbitmap: Some(bitmap),
        amendscan: Some(end_scan),
        ammarkpos: None,
        amrestrpos: None,
        amestimateparallelscan: Some(native::pin_scan_estimate_parallel),
        aminitparallelscan: Some(native::pin_scan_init_parallel),
        amparallelrescan: Some(native::pin_scan_parallel_rescan),
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
    let mut heap_tuples = 0.0;
    let mut documents = 0u64;
    let participant_memory = matching::stored(matching::build_participant_memory());
    // safety: core owns the live build descriptor and requested worker count.
    // PostgreSQL restores worker transaction state; Pin bounds participant memory.
    let parallel = unsafe {
        native::call(|| {
            native::pin_parallel_build(
                heap,
                index,
                info,
                matching::PREPARE_MEMORY as u64,
                participant_memory as u64,
                &mut heap_tuples,
                &mut documents,
            )
        })
    };
    if !parallel {
        let mut state = BuildState { heap, documents: 0 };
        let state_ptr = std::ptr::from_mut(&mut state).cast::<c_void>();
        // safety: the core scan maps HOT roots and evaluates index expressions/predicates.
        // state lives through all sequential, guarded callback invocations.
        heap_tuples = unsafe {
            native::call(|| {
                native::pin_heap_build_scan(heap, index, info, Some(build_tuple), state_ptr)
            })
        };
        documents = state.documents;
    }
    // safety: palloc returns aligned PostgreSQL-owned memory; both fields are initialized.
    unsafe {
        let result = pg_sys::palloc(std::mem::size_of::<pg_sys::IndexBuildResult>())
            .cast::<pg_sys::IndexBuildResult>();
        result.write(pg_sys::IndexBuildResult {
            heap_tuples,
            index_tuples: documents as f64,
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
    if unsafe {
        insert_value(
            index,
            state.heap,
            values,
            nulls,
            tid,
            matching::PREPARE_MEMORY,
            std::ptr::null_mut(),
        )
    } {
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
    memory_bytes: usize,
    parallel_writer: *mut c_void,
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
    let document = matching::input(PreparedDocument::prepare(&analyzed, memory_bytes));
    drop(analyzed);
    // safety: C reads a valid core-owned item pointer without changing its root identity.
    let root = matching::stored(unsafe { storage::root(tid) });
    // safety: analysis/allocation precede both writer locks.
    if !parallel_writer.is_null() {
        // safety: c owns the live dsm lwlock for this synchronous build callback.
        unsafe { native::call(|| native::pin_parallel_build_writer_lock(parallel_writer)) };
    }
    // safety: the relation and prepared document stay live through this synchronous insert.
    let result =
        unsafe { storage::with_writer(index, |store| mutable::insert(store, root, &document)) };
    if !parallel_writer.is_null() {
        // safety: this participant acquired the DSM lock immediately above.
        unsafe { native::call(|| native::pin_parallel_build_writer_unlock(parallel_writer)) };
    }
    matching::stored(result);
    true
}

/// Inserts one tuple from a PostgreSQL parallel build participant.
///
/// # Safety
/// all pointers belong to one live table build callback in this worker.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_parallel_build_tuple(
    index: pg_sys::Relation,
    heap: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    nulls: *mut bool,
    memory_bytes: u64,
    writer_lock: *mut c_void,
) -> bool {
    let memory_bytes = matching::stored(
        usize::try_from(memory_bytes)
            .map_err(|_| Error::Limit("parallel build memory"))
            .and_then(|bytes| {
                if bytes == matching::PREPARE_MEMORY {
                    Ok(bytes)
                } else {
                    Err(Error::InvalidState)
                }
            }),
    );
    if writer_lock.is_null() {
        matching::stored::<()>(Err(Error::InvalidState));
    }
    // safety: C forwards callback arguments and an opaque DSM LWLock pointer.
    unsafe { insert_value(index, heap, values, nulls, tid, memory_bytes, writer_lock) }
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
    unsafe {
        insert_value(
            index,
            heap,
            values,
            nulls,
            heap_tid,
            matching::PREPARE_MEMORY,
            std::ptr::null_mut(),
        )
    };
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
    let (index, heap, strategy) = unsafe { ((*info).index, (*info).heaprel, (*info).strategy) };
    // safety: only live relation pointers enter the checked C compatibility gate.
    unsafe { native::call(|| native::pin_storage_check(index, heap, std::ptr::null_mut())) };
    // safety: the writer interlock serializes owner liveness and free-list changes.
    let result = matching::stored(unsafe {
        storage::with_vacuum(index, strategy, |store| {
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
    let (index, heap, strategy) = unsafe { ((*info).index, (*info).heaprel, (*info).strategy) };
    // safety: only live relation pointers enter the checked C compatibility gate.
    unsafe { native::call(|| native::pin_storage_check(index, heap, std::ptr::null_mut())) };
    // safety: physical posting reclamation waits for readers, then excludes writers.
    let (compacted, pages) = matching::stored(unsafe {
        storage::with_maintenance(index, strategy, |store| {
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
    // safety: core owns scan and key arrays; c bounds the copy and preserves null keys.
    unsafe { native::call(|| native::pin_scan_rescan(scan, keys, key_count, orderby_count)) };
    // safety: plain scans retain the structural barrier acquired by the c rescan.
    unsafe { prepare_tuple_scan(scan) };
}

unsafe fn chosen_query(scan: pg_sys::IndexScanDesc) -> Option<Query> {
    // safety: core owns the descriptor, current mvcc snapshot and key array.
    unsafe { native::call(|| native::pin_scan_validate(scan)) };
    // safety: c validated the dimensions; copy scalars rather than retain field borrows.
    let (key_count, keys) = unsafe { ((*scan).numberOfKeys, (*scan).keyData) };
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
            return None;
        }
        // safety: the registered operator accepts the validated bytea query domain.
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
    Some(matching::input(chosen.ok_or(Error::InvalidParameters)))
}

unsafe fn prepare_tuple_scan(scan: pg_sys::IndexScanDesc) {
    // safety: core keeps the scan descriptor live for this callback.
    if unsafe { (*scan).heapRelation.is_null() } {
        return;
    }
    // safety: c reads only backend-local or dsm scalar coordination state.
    if unsafe { native::call(|| native::pin_scan_work_ready(scan)) } {
        return;
    }
    // safety: chosen_query borrows only this callback's live scan keys.
    let query = unsafe { chosen_query(scan) };
    // safety: core keeps the index relation open through the scan.
    let index = unsafe { (*scan).indexRelation };
    let state = match query {
        Some(query) => matching::stored(unsafe {
            storage::with_locked_reader(index, |store| {
                WorkState::capture(store, &query, matching::QUERY_MEMORY)
            })
        }),
        None => WorkState::DONE,
    };
    let words = state.words();
    // safety: c copies exactly work_words scalar words synchronously.
    unsafe {
        native::call(|| native::pin_scan_work_publish(scan, words.as_ptr(), WORK_WORDS as u32))
    };
}

/// Fills one claimed plain-index batch with live canonical heap roots.
///
/// # Safety
/// index and scan are live for one plain index scan. blocks and offsets are
/// distinct writable arrays of capacity elements and remain live for this call.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_parallel_scan_fill(
    index: pg_sys::Relation,
    scan: pg_sys::IndexScanDesc,
    blocks: *mut u32,
    offsets: *mut u16,
    capacity: u32,
) -> u32 {
    if index.is_null() || scan.is_null() || blocks.is_null() || offsets.is_null() || capacity == 0 {
        matching::stored::<()>(Err(Error::InvalidState));
    }
    let mut count = 0usize;
    let capacity = capacity as usize;
    // safety: c retains the shared structural barrier across the complete scan.
    matching::stored(unsafe {
        storage::with_locked_reader(index, |store| {
            loop {
                let mut words = [0u64; WORK_WORDS];
                native::call(|| {
                    native::pin_scan_work_snapshot(scan, words.as_mut_ptr(), WORK_WORDS as u32)
                });
                let state = WorkState::from_words(words)?;
                let Some((next, batch)) = state.prepare(store)? else {
                    break;
                };
                let next_words = next.words();
                if !native::call(|| {
                    native::pin_scan_work_claim(
                        scan,
                        words.as_ptr(),
                        next_words.as_ptr(),
                        WORK_WORDS as u32,
                    )
                }) {
                    continue;
                }
                #[cfg(feature = "test-hooks")]
                {
                    // safety: the test-only c hook accepts this fixed stage synchronously.
                    native::call(|| native::pin_parallel_test_event(19));
                }
                batch.for_each_root(store, |root| {
                    if count >= capacity {
                        return Err(Error::Limit("parallel scan batch"));
                    }
                    // safety: count is bounded by both caller-provided writable arrays.
                    blocks.add(count).write(root.block());
                    offsets.add(count).write(root.offset());
                    count += 1;
                    Ok(())
                })?;
                if count != 0 {
                    break;
                }
            }
            Ok(())
        })
    });
    matching::stored(u32::try_from(count).map_err(|_| Error::Limit("parallel scan batch")))
}

#[pg_guard]
unsafe extern "C-unwind" fn bitmap(
    scan: pg_sys::IndexScanDesc,
    bitmap: *mut pg_sys::TIDBitmap,
) -> i64 {
    // safety: chosen_query borrows only live scan-key values.
    let Some(query) = (unsafe { chosen_query(scan) }) else {
        return 0;
    };
    // safety: core keeps the index relation live through this callback.
    let index = unsafe { (*scan).indexRelation };
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
    // safety: c releases the scan's structural barrier and context-owned state.
    unsafe { native::call(|| native::pin_scan_end(scan)) };
}
