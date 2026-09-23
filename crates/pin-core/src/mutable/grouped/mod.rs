//! physical grouped snapshots over the existing checked page-store boundary.
//! maintenance requires the exclusive structural barrier before the writer lock.

mod build;
mod storage;
mod scan;
mod vacuum;

pub use scan::scan_query;
pub use build::{BuildStats, GroupSort, SORT_BATCH, SortRecord, rebuild};

pub(super) fn recover<S: super::PageStore>(store: &mut S) -> crate::error::Result<u32> {
    storage::recover(store)
}

pub(super) fn retire<S: super::PageStore>(
    store: &mut S, removable: impl FnMut(crate::identity::RootTid) -> crate::error::Result<bool>,
) -> crate::error::Result<u64> {
    vacuum::retire(store, removable)
}

pub(super) fn references(page: &super::page::Page, target: u32) -> crate::error::Result<bool> {
    vacuum::references(page, target)
}
