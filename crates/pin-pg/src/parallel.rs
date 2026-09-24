//! opt-in whole-index VACUUM integration with PostgreSQL-managed workers.
//! this module adds no pin-owned threads; postgres owns every worker lifecycle.
//! contracts, fallback and review gates: docs/g7-selective.md, G7VACUUM01.

use crate::{native, storage};
use pgrx::{pg_guard, pg_sys};
use pin_core::error::Result;
use pin_core::identity::HeapLayout;
use pin_core::mutable::page::Page;
use pin_core::mutable::{PageStore, Stage};
use std::ffi::c_void;

/// installs a postmaster-only gate before any backend can cache the AM routine.
///
/// # Safety
/// the guarded caller must be in validated shared-library preload initialization.
pub(crate) unsafe fn initialize() {
    // safety: preload owns GUC initialization; this trivial call retains no Rust data.
    unsafe { native::call(|| native::pin_parallel_init()) };
}

pub(crate) fn vacuum_options() -> u8 {
    // safety: guarded backend callers only read a startup-fixed, header-derived mask.
    unsafe { native::call(|| native::pin_parallel_vacuum_options()) }
}

pub(crate) struct VacuumStore<'store, 'rel> {
    inner: &'store mut storage::PgStore<'rel>,
    index: pg_sys::Relation,
    strategy: pg_sys::BufferAccessStrategy,
}

impl PageStore for VacuumStore<'_, '_> {
    fn frontier_anchors(&self) -> bool {
        self.inner.frontier_anchors()
    }

    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }

    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }

    fn read(&mut self, block: u32) -> Result<Page> {
        let index = self.index;
        let strategy = self.strategy;
        Page::read_with(block, |output| {
            let pointer = output.as_mut_ptr();
            let capacity = output.len() as u32;
            // safety: the callback retains the nullable strategy through this call;
            // C copies into one exclusive output slice and retains no pointer.
            Ok(unsafe {
                native::call(|| {
                    native::pin_storage_read_strategy(index, block, pointer, capacity, strategy)
                })
            } as usize)
        })
    }

    fn extend(&mut self) -> Result<u32> {
        self.inner.extend()
    }

    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        self.inner.commit(pages)
    }

    fn remove_owners(&mut self, page: &Page) -> Result<()> {
        self.inner.remove_owners(page)
    }

    fn interrupt(&mut self) -> Result<()> {
        // safety: traversal checks cancellation while Pin interlocks are held.
        // cost-delay sleeps run only outside those interlocks.
        unsafe { native::call(|| native::pin_storage_interrupt()) };
        Ok(())
    }

    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
}

/// postgres external worker entry for parallel index build.
///
/// # Safety
/// postgres supplies one live dsm segment and toc for this worker invocation.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_parallel_build_main(segment: *mut c_void, table: *mut c_void) {
    if segment.is_null() || table.is_null() {
        pgrx::error!("invalid Pin parallel build worker state");
    }
    // safety: c consumes both postgres-owned pointers only for this worker invocation.
    unsafe { native::call(|| native::pin_parallel_build_worker(segment, table)) };
}

/// postgres external worker entry for direct count.
///
/// # Safety
/// postgres supplies one live dsm segment and toc for this worker invocation.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_parallel_count_main(segment: *mut c_void, table: *mut c_void) {
    if segment.is_null() || table.is_null() {
        pgrx::error!("invalid Pin parallel count worker state");
    }
    // safety: c consumes both postgres-owned pointers only for this worker invocation.
    unsafe { native::call(|| native::pin_parallel_count_worker(segment, table)) };
}

/// runs one VACUUM phase with the callback-owned buffer strategy.
///
/// # Safety
/// index and strategy remain live through the synchronous guarded callback.
pub(crate) unsafe fn with_vacuum<T>(
    index: pg_sys::Relation,
    strategy: pg_sys::BufferAccessStrategy,
    operation: impl FnOnce(&mut VacuumStore<'_, '_>) -> Result<T>,
) -> Result<T> {
    // safety: delay points run before and after the Pin writer critical section.
    unsafe { native::call(|| native::pin_storage_vacuum_delay()) };
    // safety: the caller retains both host resources; the writer lock is resource-owned.
    let result = unsafe {
        storage::with_writer(index, |store| {
            let mut vacuum = VacuumStore {
                inner: store,
                index,
                strategy,
            };
            operation(&mut vacuum)
        })
    };
    // safety: no Pin interlock or buffer lock is held after with_writer returns.
    unsafe { native::call(|| native::pin_storage_vacuum_delay()) };
    result
}

/// excludes readers before running strategy-aware VACUUM cleanup.
///
/// # Safety
/// the caller holds neither Pin interlock and retains index and strategy until return.
pub(crate) unsafe fn with_maintenance<T>(
    index: pg_sys::Relation,
    strategy: pg_sys::BufferAccessStrategy,
    operation: impl FnOnce(&mut VacuumStore<'_, '_>) -> Result<T>,
) -> Result<T> {
    // safety: pay accumulated vacuum cost before taking the structural barrier.
    unsafe { native::call(|| native::pin_storage_vacuum_delay()) };
    // safety: existing maintenance enforces structural-before-writer lock order.
    let result = unsafe {
        crate::storage_impl::with_maintenance(index, |store| {
            let mut vacuum = VacuumStore {
                inner: store,
                index,
                strategy,
            };
            operation(&mut vacuum)
        })
    };
    // safety: compaction released both Pin interlocks before this delay point.
    unsafe { native::call(|| native::pin_storage_vacuum_delay()) };
    result
}
