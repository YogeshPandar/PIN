//! narrow C declarations; all throwing calls pass through the pgrx FFI boundary.
//! signatures match the cshim headers and the pinned PostgreSQL 18.6 headers.
//! no closure passed to call may allocate, panic, or own a destructor-bearing value.

use pgrx::pg_sys;
use std::ffi::c_void;

// safety: fixed-width scalars and generated PostgreSQL types match the C header.
unsafe extern "C-unwind" {
    pub(crate) fn pin_group_sort_begin(reserved_bytes: u64) -> *mut c_void;
    pub(crate) fn pin_group_sort_put(sort: *mut c_void, records: *const u8, count: u32);
    pub(crate) fn pin_group_sort_finish(sort: *mut c_void);
    pub(crate) fn pin_group_sort_read(sort: *mut c_void, records: *mut u8, capacity: u32) -> u32;
    pub(crate) fn pin_group_sort_end(sort: *mut c_void) -> bool;
    pub(crate) fn pin_parallel_init();
    pub(crate) fn pin_parallel_vacuum_options() -> u8;
    pub(crate) fn pin_parallel_build(
        heap: pg_sys::Relation,
        index: pg_sys::Relation,
        info: *mut pg_sys::IndexInfo,
        prepare_memory: u64,
        participant_memory: u64,
        heap_tuples: *mut f64,
        index_tuples: *mut u64,
    ) -> bool;
    pub(crate) fn pin_parallel_build_writer_lock(lock: *mut c_void);
    pub(crate) fn pin_parallel_build_writer_unlock(lock: *mut c_void);
    pub(crate) fn pin_parallel_build_worker(segment: *mut c_void, table: *mut c_void);
    pub(crate) fn pin_parallel_count_worker(segment: *mut c_void, table: *mut c_void);
    #[cfg(feature = "test-hooks")]
    pub(crate) fn pin_parallel_test_event(stage: u8);
    pub(crate) fn pin_storage_check(
        index: pg_sys::Relation,
        heap: pg_sys::Relation,
        info: *mut pg_sys::IndexInfo,
    );
    pub(crate) fn pin_writer_lock(index: pg_sys::Relation);
    pub(crate) fn pin_writer_unlock(index: pg_sys::Relation);
    pub(crate) fn pin_structure_lock(index: pg_sys::Relation, exclusive: bool);
    pub(crate) fn pin_structure_unlock(index: pg_sys::Relation, exclusive: bool);
    pub(crate) fn pin_storage_blocks(index: pg_sys::Relation) -> u32;
    pub(crate) fn pin_storage_extend(index: pg_sys::Relation) -> u32;
    #[link_name = "pin_storage_read"]
    fn pin_storage_read_raw(
        index: pg_sys::Relation,
        block: u32,
        out: *mut u8,
        capacity: u32,
        strategy: pg_sys::BufferAccessStrategy,
    ) -> u32;
    pub(crate) fn pin_storage_remove_owners(
        index: pg_sys::Relation,
        block: u32,
        bytes: *const u8,
        length: u32,
    );
    pub(crate) fn pin_storage_commit(
        index: pg_sys::Relation,
        count: u32,
        blocks: *const u32,
        bytes: *const *const u8,
        lengths: *const u32,
        full_images: *const bool,
    );
    pub(crate) fn pin_storage_interrupt();
    pub(crate) fn pin_storage_vacuum_delay();
    pub(crate) fn pin_root_coordinates(tid: pg_sys::ItemPointer, block: *mut u32, offset: *mut u16);
    pub(crate) fn pin_heap_build_scan(
        heap: pg_sys::Relation,
        index: pg_sys::Relation,
        info: *mut pg_sys::IndexInfo,
        callback: pg_sys::IndexBuildCallback,
        state: *mut c_void,
    ) -> f64;
    pub(crate) fn pin_bitmap_add(
        bitmap: *mut pg_sys::TIDBitmap,
        count: u32,
        blocks: *const u32,
        offsets: *const u16,
        recheck: bool,
    );
    pub(crate) fn pin_vacuum_removable(
        callback: pg_sys::IndexBulkDeleteCallback,
        state: *mut c_void,
        block: u32,
        offset: u16,
    ) -> bool;
    pub(crate) fn pin_scan_begin(
        index: pg_sys::Relation,
        nkeys: i32,
        norderbys: i32,
    ) -> pg_sys::IndexScanDesc;
    pub(crate) fn pin_scan_end(scan: pg_sys::IndexScanDesc);
    pub(crate) fn pin_scan_validate(scan: pg_sys::IndexScanDesc);
    pub(crate) fn pin_scan_rescan(
        scan: pg_sys::IndexScanDesc,
        keys: pg_sys::ScanKey,
        nkeys: i32,
        norderbys: i32,
    );
    pub(crate) fn pin_scan_estimate_parallel(
        index: pg_sys::Relation,
        nkeys: i32,
        norderbys: i32,
    ) -> usize;
    pub(crate) fn pin_scan_init_parallel(target: *mut c_void);
    pub(crate) fn pin_scan_parallel_rescan(scan: pg_sys::IndexScanDesc);
    pub(crate) fn pin_scan_work_ready(scan: pg_sys::IndexScanDesc) -> bool;
    pub(crate) fn pin_scan_work_publish(scan: pg_sys::IndexScanDesc, words: *const u64, count: u32);
    pub(crate) fn pin_scan_work_snapshot(scan: pg_sys::IndexScanDesc, words: *mut u64, count: u32);
    pub(crate) fn pin_scan_work_claim(
        scan: pg_sys::IndexScanDesc,
        expected: *const u64,
        next: *const u64,
        count: u32,
    ) -> bool;
    pub(crate) fn pin_scan_gettuple(
        scan: pg_sys::IndexScanDesc,
        direction: pg_sys::ScanDirection::Type,
    ) -> bool;
    pub(crate) fn pin_opclass_validate(opclass: pg_sys::Oid) -> bool;
    pub(crate) fn pin_opclass_adjust(
        opclass: pg_sys::Oid,
        operators: *mut pg_sys::List,
        functions: *mut pg_sys::List,
    );
    pub(crate) fn pin_index_cost(
        root: *mut pg_sys::PlannerInfo,
        path: *mut pg_sys::IndexPath,
        loops: f64,
        startup: *mut f64,
        total: *mut f64,
        selectivity: *mut f64,
        correlation: *mut f64,
        pages: *mut f64,
    );
}

/// Reads through the default PostgreSQL buffer strategy.
///
/// # Safety
/// The caller supplies a live relation and one exclusive output extent.
pub(crate) unsafe fn pin_storage_read(
    index: pg_sys::Relation,
    block: u32,
    out: *mut u8,
    capacity: u32,
) -> u32 {
    // safety: the caller proves the raw FFI extent and relation contract.
    unsafe { pin_storage_read_raw(index, block, out, capacity, std::ptr::null_mut()) }
}

/// Reads through a callback-owned PostgreSQL buffer strategy.
///
/// # Safety
/// The caller supplies the same raw extent plus a NULL or live callback strategy.
pub(crate) unsafe fn pin_storage_read_strategy(
    index: pg_sys::Relation,
    block: u32,
    out: *mut u8,
    capacity: u32,
    strategy: pg_sys::BufferAccessStrategy,
) -> u32 {
    // safety: the caller proves the raw FFI extent, relation and strategy contract.
    unsafe { pin_storage_read_raw(index, block, out, capacity, strategy) }
}

/// catches PostgreSQL ERROR before it crosses destructor-bearing Rust frames.
///
/// # Safety
/// the backend main thread must be inside a pg_guard entry. the closure must
/// contain only the C call, capture trivially droppable inputs, and not panic.
/// all input pointer, output extent, lock and lifetime obligations remain local.
pub(crate) unsafe fn call<T>(operation: impl FnOnce() -> T) -> T {
    // safety: the caller supplies the exact non-panicking, trivial closure contract.
    unsafe { pg_sys::ffi::pg_guard_ffi_boundary(operation) }
}
