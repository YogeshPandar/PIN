//! default-off grouped maintenance and bitmap scans over postgres-owned resources.
//! contracts: docs/g9-integration.md and the g9pg01 api evidence entry.

use crate::native;
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pin_core::error::{Error, Result};
use pin_core::identity::RootTid;
use pin_core::mutable::grouped::{self, BuildStats, GroupSort, SORT_BATCH, SortRecord};
use pin_core::mutable::{self, PageStore};
use pin_core::query::Query;
use std::ffi::c_void;
use std::ptr::NonNull;

static ENABLE_STORAGE: GucSetting<bool> = GucSetting::<bool>::new(false);
static ENABLE_SCAN: GucSetting<bool> = GucSetting::<bool>::new(false);
static ENABLE_FRONTIER_ANCHORS: GucSetting<bool> = GucSetting::<bool>::new(false);
static ENABLE_OWNER_FRONTIER: GucSetting<bool> = GucSetting::<bool>::new(false);
static ENABLE_DELTA_SEAL: GucSetting<bool> = GucSetting::<bool>::new(false);

const DELTA_SEAL_OWNERS: u64 = 512;

const _: () = assert!(core::mem::size_of::<SortRecord>() == 32);
const _: () = assert!(core::mem::align_of::<SortRecord>() == 1);
const _: () = assert!(SORT_BATCH == 256);

pub(crate) fn initialize() {
    GucRegistry::define_bool_guc(
        c"pin.enable_frontier_anchors",
        c"Build and use experimental snapshot posting-chain seek anchors.",
        c"Requires grouped storage/scan. Off uses the historical frontier; binary downgrade requires rebuilding anchored indexes.",
        &ENABLE_FRONTIER_ANCHORS,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pin.enable_owner_frontier",
        c"Use one-pass owner payloads for dense grouped write frontiers.",
        c"Requires grouped scans. Off retains term-addressed canonical frontier execution.",
        &ENABLE_OWNER_FRONTIER,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pin.enable_grouped_delta_seal",
        c"Seal bounded write suffixes into experimental immutable grouped segments.",
        c"Requires grouped storage. Off retains the historical mutable frontier and does not create PG10 metadata.",
        &ENABLE_DELTA_SEAL,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pin.enable_grouped_storage",
        c"Build experimental generation-safe page groups during CREATE INDEX and VACUUM.",
        c"Off stops new builds; existing grouped pages still require this binary for VACUUM.",
        &ENABLE_STORAGE,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pin.enable_grouped_scan",
        c"Use experimental scalar page-group pruning for Boolean bitmap scans.",
        c"Legacy fallback, heap visibility and required predicate rechecks remain enabled.",
        &ENABLE_SCAN,
        GucContext::Suset,
        GucFlags::default(),
    );
}

pub(crate) fn frontier_anchors_enabled() -> bool {
    ENABLE_FRONTIER_ANCHORS.get()
}

pub(crate) fn owner_frontier_enabled() -> bool {
    ENABLE_OWNER_FRONTIER.get()
}

pub(crate) fn storage_enabled() -> bool {
    ENABLE_STORAGE.get()
}

pub(crate) fn delta_seal_enabled() -> bool {
    storage_enabled() && ENABLE_DELTA_SEAL.get()
}

pub(crate) fn delta_due<S: PageStore>(store: &mut S) -> Result<bool> {
    Ok(!matches!(
        grouped::delta_maintenance(store, DELTA_SEAL_OWNERS)?,
        grouped::DeltaMaintenance::None
    ))
}

// this state never escapes one guarded maintenance callback or calls postgres in drop.
struct PgSort {
    pointer: NonNull<c_void>,
}

impl PgSort {
    fn begin(memory: usize) -> Option<Self> {
        let reserved = memory as u64;
        // safety: a guarded backend supplies scalar bytes; c owns the sort and tapes.
        NonNull::new(unsafe { native::call(|| native::pin_group_sort_begin(reserved)) })
            .map(|pointer| Self { pointer })
    }

    fn close(self) -> bool {
        let pointer = self.pointer.as_ptr();
        // safety: this consumes the unique live handle; no sort datum escapes read.
        unsafe { native::call(|| native::pin_group_sort_end(pointer)) }
    }
}

impl GroupSort for PgSort {
    fn put(&mut self, records: &[SortRecord]) -> Result<()> {
        if records.len() > SORT_BATCH {
            return Err(Error::InvalidState);
        }
        let pointer = self.pointer.as_ptr();
        let bytes = records.as_ptr().cast::<u8>();
        let count = records.len() as u32;
        // safety: repr-transparent records cover count * 32 bytes; c copies each one.
        unsafe { native::call(|| native::pin_group_sort_put(pointer, bytes, count)) };
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let pointer = self.pointer.as_ptr();
        // safety: the fresh host sort transitions once; c rejects repeated finish.
        unsafe { native::call(|| native::pin_group_sort_finish(pointer)) };
        Ok(())
    }

    fn read(&mut self, output: &mut [SortRecord]) -> Result<usize> {
        if output.len() > SORT_BATCH {
            return Err(Error::InvalidState);
        }
        let pointer = self.pointer.as_ptr();
        let bytes = output.as_mut_ptr().cast::<u8>();
        let capacity = output.len() as u32;
        // safety: the exclusive initialized output covers capacity * 32 bytes.
        let count =
            unsafe { native::call(|| native::pin_group_sort_read(pointer, bytes, capacity)) };
        if count > capacity {
            return Err(Error::InvalidState);
        }
        Ok(count as usize)
    }
}

// the caller holds exclusive structure then writer barriers through publication.
pub(crate) fn rebuild<S: PageStore>(store: &mut S) -> Result<BuildStats> {
    if !storage_enabled() || !grouped::needs_rebuild(store)? {
        return Ok(BuildStats::default());
    }
    let memory = if store.frontier_anchors() {
        grouped::build_memory_with_anchors(store.layout())
    } else {
        grouped::build_memory(store.layout())
    };
    let Some(mut sort) = PgSort::begin(memory) else {
        pgrx::pg_sys::debug1!("Pin grouped snapshot skipped: maintenance memory is too small");
        return Ok(BuildStats::default());
    };
    let result = grouped::rebuild(store, &mut sort, memory);
    // release sort resources before propagating an ordinary core error.
    let spilled = sort.close();
    let stats = result?;
    pgrx::pg_sys::debug1!(
        "Pin grouped snapshot: documents={} groups={} written_pages={} reclaimed_pages={} sort_spilled={} frontier_terms={}",
        stats.documents,
        stats.groups,
        stats.written_pages,
        stats.reclaimed_pages,
        spilled,
        stats.frontier_terms,
    );
    Ok(stats)
}

// the caller holds exclusive structure then writer barriers through publication.
pub(crate) fn maintain_delta<S: PageStore>(store: &mut S) -> Result<BuildStats> {
    if !delta_seal_enabled() {
        return Ok(BuildStats::default());
    }
    match grouped::delta_maintenance(store, DELTA_SEAL_OWNERS)? {
        grouped::DeltaMaintenance::None => Ok(BuildStats::default()),
        grouped::DeltaMaintenance::Rebuild => rebuild(store),
        grouped::DeltaMaintenance::Seal => {
            let memory = grouped::delta_build_memory(store.layout());
            let Some(mut sort) = PgSort::begin(memory) else {
                pgrx::pg_sys::debug1!("Pin grouped delta skipped: maintenance memory is too small");
                return Ok(BuildStats::default());
            };
            let result = grouped::seal_delta(store, &mut sort, memory);
            let spilled = sort.close();
            let stats = result?;
            pgrx::pg_sys::debug1!(
                "Pin grouped delta: documents={} groups={} written_pages={} reclaimed_pages={} sort_spilled={}",
                stats.documents,
                stats.groups,
                stats.written_pages,
                stats.reclaimed_pages,
                spilled,
            );
            Ok(stats)
        }
    }
}

// the host retains the structural read barrier and the existing batched bitmap sink.
pub(crate) fn scan<S: PageStore>(
    store: &mut S,
    query: &Query,
    memory: usize,
    emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<u64> {
    if ENABLE_SCAN.get() {
        grouped::scan_query(store, query, memory, emit)
    } else {
        mutable::scan_query_with_recheck(store, query, memory, emit)
    }
}
