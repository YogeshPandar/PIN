//! Bounded mixed VM-certified and heap-checked counting.
//! PostgreSQL owns the context, snapshot, locks and slots. Rust owns query and
//! page copies only. Normal exits release explicitly; ERROR uses host cleanup.
//! Contracts: docs/g5-counts.md and docs/api-evidence.md, count01.

use crate::{matching, native, storage};
use pgrx::{pg_guard, pg_sys};
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::codec::records::Publication;
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::page::{Page, PageKind};
use pin_core::mutable::{self, CountCandidate};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use std::ffi::c_void;

const BATCH: usize = 64;
const COUNTERS: usize = 8;

unsafe extern "C-unwind" {
    fn pin_count_init();
    fn pin_count_owner_lock(context: *mut c_void, block: u32, out: *mut u8, capacity: u32) -> u32;
    fn pin_count_owner_unlock(context: *mut c_void);
    fn pin_count_all_visible(context: *mut c_void, block: u32) -> bool;
    fn pin_count_fetch(context: *mut c_void, block: u32, offset: u16,
                       bytes: *mut *const u8, length: *mut usize) -> bool;
    fn pin_count_clear(context: *mut c_void);
}

/// # Safety
/// called once during validated postmaster preloading on the backend main thread.
pub(crate) unsafe fn initialize() {
    // safety: static C methods and the GUC outlive every inherited backend.
    unsafe { native::call(|| pin_count_init()) };
}

struct Counter<'q> {
    context: *mut c_void,
    query: &'q Query,
    layout: HeapLayout,
    pending: [Option<CountCandidate>; BATCH],
    length: usize,
    stats: [u64; COUNTERS],
}

impl Counter<'_> {
    fn note(&mut self, counter: usize, amount: u64) -> Result<()> {
        self.stats[counter] = self.stats[counter]
            .checked_add(amount)
            .ok_or(Error::Limit("count instrumentation"))?;
        Ok(())
    }

    fn push(&mut self, candidate: CountCandidate) -> Result<()> {
        if self.length == BATCH || self.pending[0].is_some_and(|first| {
            first.owner.page != candidate.owner.page
        }) {
            self.flush()?;
        }
        self.pending[self.length] = Some(candidate);
        self.length += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.length == 0 {
            return Ok(());
        }
        storage::interrupt();
        let block = self.pending[0].ok_or(Error::InvalidState)?.owner.page;
        let context = self.context;
        let mut fallback: [Option<RootTid>; BATCH] = [None; BATCH];
        let mut fetches = 0;
        // pure errors leave the native owner pin releasable after this closure.
        let result = (|| -> Result<()> {
            let page = Page::read_with(block, |bytes| {
                let pointer = bytes.as_mut_ptr();
                let capacity = bytes.len() as u32;
                // safety: C copies under a shared content lock, then retains only
                // its resource-owned buffer pin until pin_count_owner_unlock.
                Ok(unsafe {
                    native::call(|| pin_count_owner_lock(context, block, pointer, capacity))
                } as usize)
            })?;
            page.validate(self.layout)?;
            if page.kind() != PageKind::Owners {
                return Err(Error::InvalidState);
            }
            self.note(1, 1)?;
            for index in 0..self.length {
                let candidate = self.pending[index].ok_or(Error::InvalidState)?;
                self.note(0, 1)?;
                let owner = page.owner(candidate.owner.slot, self.layout)?;
                if owner.reference != candidate.owner {
                    return Err(Error::InvalidState);
                }
                if !owner.live || owner.publication != Publication::Published {
                    self.note(2, 1)?;
                    continue;
                }
                if candidate.sealed_term {
                    self.note(3, 1)?;
                    let heap_block = owner.root.block();
                    // safety: the canonical owner pin spans the fresh VM decision.
                    if unsafe { native::call(|| pin_count_all_visible(context, heap_block)) } {
                        self.note(4, 1)?;
                        continue;
                    }
                } else {
                    self.note(7, 1)?;
                }
                fallback[fetches] = Some(owner.root);
                fetches += 1;
            }

            for root in &fallback[..fetches] {
                self.note(5, 1)?;
                let root = root.ok_or(Error::InvalidState)?;
                let block = root.block();
                let offset = root.offset();
                let mut bytes = std::ptr::null();
                let mut length = 0;
                // safety: the owner pin still blocks cleanup while C follows the
                // root's HOT chain under the active MVCC snapshot.
                let visible = unsafe {
                    native::call(|| pin_count_fetch(context, block, offset, &mut bytes, &mut length))
                };
                let matched = if visible {
                    if bytes.is_null() || length > isize::MAX as usize {
                        Err(Error::InvalidState)
                    } else {
                        // safety: C returns initialized text bytes valid until clear.
                        let value = unsafe { std::slice::from_raw_parts(bytes, length) };
                        std::str::from_utf8(value)
                            .map_err(|_| Error::InvalidDocument)
                            .and_then(|text| Analyzed::analyze(text, AnalysisLimits::default()))
                            .and_then(|document| {
                                oracle::matches(
                                    &document,
                                    self.query,
                                    matching::QUERY_MEMORY,
                                    matching::MATCH_STEPS,
                                )
                            })
                    }
                } else {
                    Ok(false)
                };
                // safety: every borrow above ended before the slot/context reset.
                unsafe { native::call(|| pin_count_clear(context)) };
                if matched? {
                    self.note(6, 1)?;
                }
            }
            Ok(())
        })();

        // safety: C retains at most one owner pin; release is idempotent.
        unsafe { native::call(|| pin_count_owner_unlock(context)) };
        result?;
        self.pending.fill(None);
        self.length = 0;
        Ok(())

    }
}

/// Executes one count to completion without retaining Rust state in a plan.
///
/// # Safety
/// C supplies an open validated index, live count context with a supported MVCC
/// snapshot, immutable query bytes in one allocation, and exclusive writable
/// storage for COUNTERS aligned u64 values. All remain live through this call.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_count_execute(
    index: pg_sys::Relation,
    context: *mut c_void,
    bytes: *const u8,
    length: usize,
    counters: *mut u64,
) -> i64 {
    crate::compatibility::database();
    if index.is_null() || context.is_null() || bytes.is_null() || counters.is_null()
        || length > isize::MAX as usize
    {
        matching::stored::<()>(Err(Error::InvalidState));
    }
    // safety: the C caller supplies one immutable detoasted query allocation;
    // no resettable per-tuple context owns these bytes.
    let input = unsafe { std::slice::from_raw_parts(bytes, length) };
    let query = matching::input(Query::decode(input, QueryLimits::default()));
    let layout = matching::stored(HeapLayout::new(crate::abi::constant(9) as u16)
        .map_err(|_| Error::InvalidState));
    let mut counter = Counter {
        context, query: &query, layout, pending: [None; BATCH], length: 0,
        stats: [0; COUNTERS],
    };
    // safety: C retains the index lock; the structural barrier prevents sealed
    // payload replacement while fresh canonical owner checks handle VACUUM.
    matching::stored(unsafe { storage::with_reader(index, |store| {
        mutable::scan_count(store, &query, |candidate| counter.push(candidate))?;
        counter.flush()
    }) });
    let count = matching::stored(counter.stats[4].checked_add(counter.stats[6])
        .and_then(|count| i64::try_from(count).ok())
        .ok_or(Error::Limit("SQL count")));
    for (position, value) in counter.stats.iter().enumerate() {
        // safety: caller reserved COUNTERS aligned writable u64s; no C code
        // accesses them concurrently with this synchronous guarded invocation.
        unsafe { counters.add(position).write(*value) };
    }
    count
}
