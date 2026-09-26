//! synchronous v2 direct build adapter; publication happens after sorted output is durable.

use crate::{matching, native, primary_sort::PgPrimarySort, storage};
use pgrx::{FromDatum, pg_guard, pg_sys};
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::identity::{Generation, SegmentId};
use pin_core::mutable::{
    PageStore,
    page::{PRIMARY_PAYLOAD_BYTES, Page},
};
use pin_core::primary::{
    BuildReducer, CatalogueBuilder, CatalogueFence, EncodedCataloguePage, GroupAddress,
    MAX_SORT_RECORD_BYTES, ManifestRootBuilder, PrimaryBuildWriter, PrimaryRoot, TermSortRecord,
    encode_manifest_leaf,
};
use std::ffi::c_void;

const PIPELINE_MEMORY: usize = 9 << 20;
// Root page bounds leaf count to floor((8152 - 36) / (16 + 1 + 1)) = 450;
// each retained encoded leaf is at most 8152 bytes, so 4 MiB covers payloads and descriptors.
const MANIFEST_LEAF_RETENTION: usize = 4 << 20;
const LEAF_HEADER_BYTES: usize = 36;
const LEAF_ENTRY_BYTES: usize = 16;

struct ScanState {
    sorter: *mut PgPrimarySort,
    documents: u64,
}

struct Publication {
    relation: Generation,
    segment: SegmentId,
    epoch: u64,
    catalogue: CatalogueBuilder,
    leaf_fences: Vec<CatalogueFence>,
    leaf_size: usize,
    manifest: ManifestRootBuilder,
    catalogue_pages: u64,
    catalogue_rows: u64,
    manifest_leaves: u64,
}

/// PostgreSQL AM build callback for the experimental immutable v2 route.
///
/// # Safety
/// core supplies open, locked relations and a live build descriptor for this call.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn build(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    crate::compatibility::database();
    // safety: core keeps the build relations and descriptor live through this check.
    unsafe { native::call(|| native::pin_storage_check(index, heap, info)) };
    // safety: the index relation is live for the duration of this AM build callback.
    let oid = unsafe { (*index).rd_id };
    let relation = matching::input(
        Generation::new(u64::from(u32::from(oid))).map_err(|_| Error::InvalidState),
    );
    let segment = matching::input(SegmentId::new(1).map_err(|_| Error::InvalidState));
    // safety: core supplied the live index relation; storage access stays within this callback.
    matching::stored(unsafe {
        storage::with_writer(index, |store| {
            pin_core::primary::initialize(store, relation)
        })
    });

    let reserved = matching::stored(matching::build_participant_memory().and_then(|bytes| {
        bytes
            .checked_add(PIPELINE_MEMORY + MANIFEST_LEAF_RETENTION)
            .ok_or(Error::Limit("v2 build memory"))
    }));
    let Some(mut sorter) = PgPrimarySort::begin(reserved) else {
        matching::input::<()>(Err(Error::Limit(
            "maintenance_work_mem for v2 direct build",
        )));
        unreachable!();
    };
    let mut scan = ScanState {
        sorter: &mut sorter,
        documents: 0,
    };
    let scan_ptr = std::ptr::from_mut(&mut scan).cast::<c_void>();
    // safety: the callback state and sorter remain live for the synchronous heap scan.
    let heap_tuples = unsafe {
        native::call(|| native::pin_heap_build_scan(heap, index, info, Some(scan_tuple), scan_ptr))
    };
    let documents = scan.documents;
    if let Err(error) = sorter.finish() {
        let _ = sorter.close();
        return matching::stored(Err(error));
    }
    // safety: the index relation and finished sorter remain live throughout synchronous output.
    let result = unsafe {
        storage::with_writer(index, |store| {
            build_sorted(store, relation, segment, &mut sorter)
        })
    };
    let spilled = sorter.close();
    let (catalogue_pages, manifest_leaves) = matching::stored(result);
    pgrx::pg_sys::debug1!(
        "Pin v2 build: documents={} catalogue_pages={} manifest_leaves={} sort_spilled={}",
        documents,
        catalogue_pages,
        manifest_leaves,
        spilled,
    );
    // safety: palloc returns aligned PostgreSQL-owned storage for both initialized fields.
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

/// v2 does not yet support unlogged init forks.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn build_empty(_index: pg_sys::Relation) {
    pgrx::ereport!(
        ERROR,
        pgrx::PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
        "Pin v2 does not support unlogged indexes"
    );
}

#[pg_guard]
unsafe extern "C-unwind" fn scan_tuple(
    _index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    nulls: *mut bool,
    _tuple_is_alive: bool,
    state: *mut c_void,
) {
    // safety: table build scan invokes this once at a time with the live state pointer.
    let state = unsafe { &mut *state.cast::<ScanState>() };
    storage::interrupt();
    // safety: the validated text opclass supplies one initialized value and null flag.
    if unsafe { *nulls } {
        return;
    }
    // safety: the opclass fixes the input type to text; the value is consumed synchronously.
    let text = unsafe { <&str as FromDatum>::from_datum(*values, false) };
    let text = matching::input(text.ok_or(Error::InvalidDocument));
    let analyzed = matching::input(Analyzed::analyze(text, AnalysisLimits::default()));
    // safety: C extracts a checked HOT root coordinate from this callback TID.
    let root = matching::stored(unsafe { storage::root(tid) });
    // safety: scan_tuple runs synchronously while build retains the unique mutable sorter borrow.
    let sorter = unsafe { state.sorter.as_mut() }.ok_or(Error::InvalidState);
    let sorter = matching::stored(sorter);
    matching::stored(TermSortRecord::visit_document(
        &analyzed,
        root,
        matching::PREPARE_MEMORY,
        |record| sorter.put(record),
    ));
    state.documents = matching::stored(state.documents.checked_add(1).ok_or(Error::InvalidState));
}

fn build_sorted<S: PageStore>(
    store: &mut S,
    relation: Generation,
    segment: SegmentId,
    sorter: &mut PgPrimarySort,
) -> Result<(u64, u64)> {
    let layout = store.layout();
    let epoch = 2;
    let mut reducer = BuildReducer::new(layout)?;
    let mut writer = PrimaryBuildWriter::new(store, relation, segment)?;
    let mut publication = Publication {
        relation,
        segment,
        epoch,
        catalogue: CatalogueBuilder::new(1)?,
        leaf_fences: Vec::new(),
        leaf_size: LEAF_HEADER_BYTES,
        manifest: ManifestRootBuilder::new(relation, layout, epoch)?,
        catalogue_pages: 0,
        catalogue_rows: 0,
        manifest_leaves: 0,
    };

    let mut encoded = [0u8; MAX_SORT_RECORD_BYTES];
    while let Some(record) = sorter.read(&mut encoded, layout)? {
        reducer.push(record, |term, base, page, offsets| {
            writer.push(
                term,
                base,
                page,
                offsets,
                |store, term, key, ordinal, extent| {
                    write_catalogue_row(store, &mut publication, term, ordinal, key, extent)
                },
            )
        })?;
    }
    reducer.finish(|term, base, page, offsets| {
        writer.push(
            term,
            base,
            page,
            offsets,
            |store, term, key, ordinal, extent| {
                write_catalogue_row(store, &mut publication, term, ordinal, key, extent)
            },
        )
    })?;
    writer.finish(|store, term, key, ordinal, extent| {
        write_catalogue_row(store, &mut publication, term, ordinal, key, extent)
    })?;

    if publication.catalogue_rows != 0 {
        let block = store.extend()?;
        let page = publication.catalogue.finish_page_at(block)?;
        commit_catalogue_page(store, &mut publication, page)?;
    }
    if !publication.leaf_fences.is_empty() {
        commit_manifest_leaf(store, &mut publication)?;
    }
    if publication.manifest_leaves != 0 {
        let root_block = store.extend()?;
        let manifest = publication.manifest.finish(root_block)?;
        let root = PrimaryRoot {
            relation,
            layout,
            epoch,
            manifest_block: root_block,
            segment_count: 1,
        };
        let manifest_page = Page::primary(root_block, &manifest.root_payload)?;
        let metadata_page = Page::primary_metadata(root)?;
        // One WAL commit publishes the new manifest root and its catalog pointer together.
        store.commit(&[&manifest_page, &metadata_page])?;
    }
    Ok((publication.catalogue_pages, publication.manifest_leaves))
}

fn write_catalogue_row<S: PageStore>(
    store: &mut S,
    state: &mut Publication,
    term: &str,
    ordinal: u64,
    key: pin_core::grouped::GroupKey,
    extent: pin_core::primary::Extent,
) -> Result<()> {
    let group = GroupAddress {
        group_base: key.base(),
        block: extent.block,
        offset: extent.offset,
        len: extent.len,
    };
    if state.catalogue.try_push(term.as_bytes(), ordinal, group)? {
        state.catalogue_rows = state
            .catalogue_rows
            .checked_add(1)
            .ok_or(Error::InvalidState)?;
        return Ok(());
    }
    let block = store.extend()?;
    let encoded = state.catalogue.finish_page_at(block)?;
    commit_catalogue_page(store, state, encoded)?;
    if !state.catalogue.try_push(term.as_bytes(), ordinal, group)? {
        return Err(Error::InvalidState);
    }
    state.catalogue_rows = state
        .catalogue_rows
        .checked_add(1)
        .ok_or(Error::InvalidState)?;
    Ok(())
}

fn commit_catalogue_page<S: PageStore>(
    store: &mut S,
    state: &mut Publication,
    encoded: EncodedCataloguePage,
) -> Result<()> {
    let page = Page::primary(encoded.fence.block, &encoded.payload)?;
    store.commit(&[&page])?;
    state.catalogue_pages = state
        .catalogue_pages
        .checked_add(1)
        .ok_or(Error::InvalidState)?;
    append_manifest_fence(store, state, encoded.fence)
}

fn append_manifest_fence<S: PageStore>(
    store: &mut S,
    state: &mut Publication,
    fence: CatalogueFence,
) -> Result<()> {
    let row_size = LEAF_ENTRY_BYTES
        .checked_add(fence.first_lexeme.len())
        .and_then(|n| n.checked_add(fence.last_lexeme.len()))
        .ok_or(Error::InvalidState)?;
    if state.leaf_size + row_size > PRIMARY_PAYLOAD_BYTES && !state.leaf_fences.is_empty() {
        commit_manifest_leaf(store, state)?;
    }
    if state.leaf_size + row_size > PRIMARY_PAYLOAD_BYTES {
        return Err(Error::Limit("manifest fence exceeds leaf capacity"));
    }
    state
        .leaf_fences
        .try_reserve(1)
        .map_err(|_| Error::Allocation)?;
    state.leaf_fences.push(fence);
    state.leaf_size += row_size;
    Ok(())
}

fn commit_manifest_leaf<S: PageStore>(store: &mut S, state: &mut Publication) -> Result<()> {
    let block = store.extend()?;
    let leaf = encode_manifest_leaf(
        state.relation,
        store.layout(),
        state.epoch,
        state.segment,
        block,
        &state.leaf_fences,
    )?;
    let page = Page::primary(block, &leaf.payload)?;
    store.commit(&[&page])?;
    state.manifest.push_leaf(leaf)?;
    state.manifest_leaves = state
        .manifest_leaves
        .checked_add(1)
        .ok_or(Error::InvalidState)?;
    state.leaf_fences.clear();
    state.leaf_size = LEAF_HEADER_BYTES;
    Ok(())
}
