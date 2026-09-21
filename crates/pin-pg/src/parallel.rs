//! opt-in whole-index maintenance using PostgreSQL's worker and DSM lifecycle.
//! no custom workers, shared Rust objects or parallel posting scans are enabled.
//! contracts, fallback and review gates: docs/g7-parallel-vacuum.md, G7PARALLEL01.

use crate::{native, storage};
use pgrx::pg_sys;
use pin_core::error::Result;
use pin_core::identity::HeapLayout;
use pin_core::mutable::page::Page;
use pin_core::mutable::{PageStore, Stage};

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
        // safety: pure page traversal calls this outside content locks and WAL batches.
        unsafe { native::call(|| native::pin_storage_vacuum_delay()) };
        Ok(())
    }

    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
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
    // safety: the caller retains both host resources; the writer lock is resource-owned.
    unsafe {
        storage::with_writer(index, |store| {
            let mut vacuum = VacuumStore {
                inner: store,
                index,
                strategy,
            };
            operation(&mut vacuum)
        })
    }
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
    // safety: existing maintenance enforces structural-before-writer lock order.
    unsafe {
        crate::storage_impl::with_maintenance(index, |store| {
            let mut vacuum = VacuumStore {
                inner: store,
                index,
                strategy,
            };
            operation(&mut vacuum)
        })
    }
}
