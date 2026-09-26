//! Experimental immutable v2 access method for paired PostgreSQL measurements.

#![allow(
    clippy::too_many_arguments,
    reason = "PostgreSQL fixes the access-method callback signatures"
)]

use crate::{am, matching, native, storage};
use pgrx::{Internal, pg_extern, pg_guard, pg_sys};
use pin_core::candidate::CandidatePlan;
use pin_core::error::Error;
use pin_core::identity::{RootTid, SegmentId};
use pin_core::mutable::PageStore;
use pin_core::primary::scan_term;

#[pg_extern(sql = r#"
CREATE FUNCTION pin.pin2_handler(internal)
RETURNS index_am_handler
LANGUAGE c VOLATILE PARALLEL UNSAFE CALLED ON NULL INPUT
AS '@MODULE_PATHNAME@', '@FUNCTION_NAME@';
"#)]
pub(crate) fn pin2_handler() -> Internal {
    let mut handler = am::pin_handler();
    // safety: pin_handler created this PostgreSQL-owned IndexAmRoutine value.
    let routine = unsafe { handler.get_mut::<pg_sys::IndexAmRoutine>() }
        .expect("pin handler must return a routine");
    routine.amcanparallel = false;
    routine.amcanbuildparallel = false;
    routine.ambuild = Some(crate::primary_build::build);
    routine.ambuildempty = Some(crate::primary_build::build_empty);
    routine.aminsert = Some(reject_insert);
    routine.aminsertcleanup = None;
    routine.ambulkdelete = Some(reject_bulk_delete);
    routine.amvacuumcleanup = Some(reject_vacuum_cleanup);
    routine.amvalidate = Some(validate_opclass);
    routine.amrescan = Some(rescan);
    routine.amgettuple = None;
    routine.amgetbitmap = Some(bitmap);
    routine.amestimateparallelscan = None;
    routine.aminitparallelscan = None;
    routine.amparallelrescan = None;
    handler
}

const MAX_BOOLEAN_ROOTS: usize = matching::QUERY_MEMORY / (3 * std::mem::size_of::<RootTid>());

/// Returns None before emitting anything if materialization would exceed the
/// bounded query scratch; the bitmap callback then uses a lossy term cover.
fn exact_boolean<S: PageStore>(
    store: &mut S,
    root: pin_core::primary::PrimaryRoot,
    segment: SegmentId,
    terms: &[&str],
    union: bool,
    sink: &mut storage::BitmapSink,
) -> pin_core::error::Result<Option<u64>> {
    let mut selected = Vec::<RootTid>::new();
    for (position, term) in terms.iter().enumerate() {
        let mut next = Vec::<RootTid>::new();
        let read = scan_term(store, root, segment, term.as_bytes(), |tid| {
            if next.len() == MAX_BOOLEAN_ROOTS {
                return Err(Error::Limit("exact Boolean scratch"));
            }
            next.try_reserve(1).map_err(|_| Error::Allocation)?;
            next.push(tid);
            Ok(())
        });
        match read {
            Err(Error::Limit("exact Boolean scratch")) => return Ok(None),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        if position == 0 {
            selected = next;
        } else if union {
            let mut combined = Vec::new();
            combined
                .try_reserve_exact(
                    selected
                        .len()
                        .saturating_add(next.len())
                        .min(MAX_BOOLEAN_ROOTS),
                )
                .map_err(|_| Error::Allocation)?;
            let mut left = 0;
            let mut right = 0;
            while left < selected.len() || right < next.len() {
                if combined.len() == MAX_BOOLEAN_ROOTS {
                    return Ok(None);
                }
                let value = match (selected.get(left), next.get(right)) {
                    (Some(a), Some(b)) if a < b => {
                        left += 1;
                        *a
                    }
                    (Some(a), Some(b)) if b < a => {
                        right += 1;
                        *b
                    }
                    (Some(a), Some(_)) => {
                        left += 1;
                        right += 1;
                        *a
                    }
                    (Some(a), None) => {
                        left += 1;
                        *a
                    }
                    (None, Some(b)) => {
                        right += 1;
                        *b
                    }
                    (None, None) => break,
                };
                combined.push(value);
            }
            selected = combined;
        } else {
            let mut left = 0;
            let mut right = 0;
            let mut written = 0;
            while left < selected.len() && right < next.len() {
                match selected[left].cmp(&next[right]) {
                    std::cmp::Ordering::Less => left += 1,
                    std::cmp::Ordering::Greater => right += 1,
                    std::cmp::Ordering::Equal => {
                        selected[written] = selected[left];
                        written += 1;
                        left += 1;
                        right += 1;
                    }
                }
            }
            selected.truncate(written);
            if selected.is_empty() {
                break;
            }
        }
    }
    let count = u64::try_from(selected.len()).map_err(|_| Error::Limit("bitmap accounting"))?;
    for tid in selected {
        sink.push(tid, false)?;
    }
    Ok(Some(count))
}

#[pg_guard]
unsafe extern "C-unwind" fn reject_insert(
    _index: pg_sys::Relation,
    _values: *mut pg_sys::Datum,
    _nulls: *mut bool,
    _tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    _index_info: *mut pg_sys::IndexInfo,
) -> bool {
    am::unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn reject_bulk_delete(
    _info: *mut pg_sys::IndexVacuumInfo,
    _stats: *mut pg_sys::IndexBulkDeleteResult,
    _callback: pg_sys::IndexBulkDeleteCallback,
    _state: *mut std::ffi::c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    am::unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn reject_vacuum_cleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    // safety: core passes a live IndexVacuumInfo to its cleanup callback.
    if !info.is_null() && unsafe { (*info).analyze_only } {
        return stats;
    }
    am::unavailable()
}

#[pg_guard]
unsafe extern "C-unwind" fn validate_opclass(opclass: pg_sys::Oid) -> bool {
    // safety: the catalog OID is checked by the native syscache routine.
    unsafe { native::call(|| native::pin2_opclass_validate(opclass)) }
}

#[pg_guard]
unsafe extern "C-unwind" fn rescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: i32,
    _orderbys: pg_sys::ScanKey,
    norderbys: i32,
) {
    // safety: the C shim owns the scan and bounds key array copying.
    unsafe { native::call(|| native::pin_scan_rescan(scan, keys, nkeys, norderbys)) };
}

#[pg_guard]
unsafe extern "C-unwind" fn bitmap(
    scan: pg_sys::IndexScanDesc,
    bitmap: *mut pg_sys::TIDBitmap,
) -> i64 {
    // safety: PostgreSQL retains the validated descriptor and its key datums.
    let Some((query, single_key)) = (unsafe { am::chosen_query(scan) }) else {
        return 0;
    };
    let exact = single_key && query.is_single_term();
    let mut exact_terms = [""; 256];
    let exact_shape = if single_key {
        query
            .exact_conjunction_terms(&mut exact_terms)
            .filter(|count| *count > 1)
            .map(|count| (count, false))
            .or_else(|| {
                query
                    .exact_disjunction_terms(&mut exact_terms)
                    .filter(|count| *count > 1)
                    .map(|count| (count, true))
            })
    } else {
        None
    };
    let plan = matching::input(CandidatePlan::build(&query, matching::QUERY_MEMORY));
    let terms = match plan {
        CandidatePlan::Empty => return 0,
        CandidatePlan::Universe => am::unavailable(),
        CandidatePlan::Terms(terms) => terms,
    };
    // safety: PostgreSQL keeps the index relation open for this callback.
    let index = unsafe { (*scan).indexRelation };
    // safety: PostgreSQL passes a live writable TIDBitmap.
    let mut sink = unsafe { storage::BitmapSink::new(bitmap) };
    // safety: PostgreSQL keeps the index relation open and locked for this bitmap callback.
    let count = matching::stored(unsafe {
        storage::with_reader(index, |store| {
            let page = store.read(0)?;
            page.validate(store.layout())?;
            let root = page.primary_root()?;
            let segment = SegmentId::new(1).map_err(|_| Error::InvalidState)?;
            if let Some((count, union)) = exact_shape
                && let Some(found) = exact_boolean(
                    store,
                    root,
                    segment,
                    &exact_terms[..count],
                    union,
                    &mut sink,
                )?
            {
                return Ok(found);
            }
            let mut count = 0u64;
            for term in terms {
                count = count
                    .checked_add(scan_term(store, root, segment, term.as_bytes(), |tid| {
                        sink.push(tid, !exact)
                    })?)
                    .ok_or(Error::Limit("bitmap accounting"))?;
            }
            Ok(count)
        })
    });
    sink.flush();
    matching::stored(i64::try_from(count).map_err(|_| Error::Limit("bitmap accounting")))
}
