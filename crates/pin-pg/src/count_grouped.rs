//! exact page-mask counts with a PostgreSQL retirement interlock and fresh VM probes.
//! only snapshot masks qualify for VM; other roots use heap visibility and any required recheck.
//! contracts, upgrade gate and qualification status: docs/grouped-count.md.

use crate::{matching, native, storage};
use pgrx::{pg_guard, pg_sys};
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::grouped::QueryStats;
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::grouped::{self, GroupSink};
use pin_core::mutable::page::Page;
use pin_core::mutable::{PageStore, Stage};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use std::ffi::c_void;

const COUNTERS: usize = 11;

// safety: these declarations match pin_count.h; every call is guarded below.
unsafe extern "C-unwind" {
    fn pin_count_group_visible(context: *mut c_void, block: u32, offset: u16) -> bool;
    fn pin_count_group_all_visible(context: *mut c_void, block: u32) -> bool;
    fn pin_count_fetch(
        context: *mut c_void,
        block: u32,
        offset: u16,
        bytes: *mut *const u8,
        length: *mut usize,
    ) -> bool;
    fn pin_count_clear(context: *mut c_void);
}

struct Counter<'q> {
    context: *mut c_void,
    query: &'q Query,
    stats: [u64; COUNTERS],
}

impl Counter<'_> {
    fn note(&mut self, counter: usize, amount: u64) -> Result<()> {
        self.stats[counter] = self.stats[counter]
            .checked_add(amount)
            .ok_or(Error::Limit("group count instrumentation"))?;
        Ok(())
    }

    fn heap(&mut self, root: RootTid, recheck: bool) -> Result<()> {
        storage::interrupt();
        self.note(4, 1)?;
        let context = self.context;
        let block = root.block();
        let offset = root.offset();
        if !recheck {
            // safety: exact membership is protected; C checks this snapshot,
            // follows HOT and clears the slot before returning the scalar result.
            let matched = unsafe {
                native::call(|| pin_count_group_visible(context, block, offset))
            };
            if matched {
                self.note(5, 1)?;
            }
            return Ok(());
        }
        let mut bytes = std::ptr::null();
        let mut length = 0;
        // safety: C retains the snapshot, retirement lock and index relation;
        // the synchronous fetch follows HOT and returns bytes until clear.
        let visible = unsafe {
            native::call(|| pin_count_fetch(context, block, offset, &mut bytes, &mut length))
        };
        let matched = if visible {
            if bytes.is_null() || length > isize::MAX as usize {
                Err(Error::InvalidState)
            } else {
                // no early return while the C-owned tuple scratch is borrowed.
                self.note(10, 1).and_then(|()| {
                    // safety: one initialized text extent remains live until clear.
                    let value = unsafe { std::slice::from_raw_parts(bytes, length) };
                    std::str::from_utf8(value)
                        .map_err(|_| Error::InvalidDocument)
                        .and_then(|text| Analyzed::analyze(text, AnalysisLimits::default()))
                        .and_then(|doc| {
                            oracle::matches(
                                &doc,
                                self.query,
                                matching::QUERY_MEMORY,
                                matching::MATCH_STEPS,
                            )
                        })
                })
            }
        } else {
            Ok(false)
        };
        // safety: all borrowed text and analyzed documents have been dropped.
        unsafe { native::call(|| pin_count_clear(context)) };
        if matched? {
            self.note(5, 1)?;
        }
        Ok(())
    }
}

impl GroupSink for Counter<'_> {
    fn root(&mut self, root: RootTid, recheck: bool) -> Result<()> {
        self.note(6, 1)?;
        self.heap(root, recheck)
    }

    fn page(&mut self, block: u32, offsets: &[u64; 8], layout: HeapLayout) -> Result<()> {
        storage::interrupt();
        self.note(0, 1)?;
        self.note(1, 1)?;
        #[cfg(feature = "test-hooks")]
        crate::test_hooks::storage_event(Stage::CountBeforeVisibility);
        let context = self.context;
        // safety: C acquired the retirement lock before the structural barrier;
        // this mask was freshly read from that protected snapshot membership.
        let visible = unsafe { native::call(|| pin_count_group_all_visible(context, block)) };
        #[cfg(feature = "test-hooks")]
        crate::test_hooks::storage_event(Stage::CountAfterVisibility);
        if visible {
            self.note(2, 1)?;
            return self.note(3, offsets.iter().map(|word| u64::from(word.count_ones())).sum());
        }
        for (word, &value) in offsets.iter().enumerate() {
            let mut pending = value;
            while pending != 0 {
                let bit = word * 64 + pending.trailing_zeros() as usize;
                pending &= pending - 1;
                let root = RootTid::new(block, (bit + 1) as u16, layout)
                    .map_err(|_| Error::InvalidState)?;
                self.heap(root, false)?;
            }
        }
        Ok(())
    }

    fn work(&mut self, stats: &QueryStats) -> Result<()> {
        self.note(7, u64::from(stats.term_bytes))
    }
}

// measurements include repeated reads and validated private bytes, not disk I/O.
struct Metered<'a, S> {
    inner: &'a mut S,
    pages: u64,
    bytes: u64,
}

impl<S: PageStore> PageStore for Metered<'_, S> {
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn frontier_anchors(&self) -> bool {
        self.inner.frontier_anchors()
    }
    fn owner_frontier(&self) -> bool {
        self.inner.owner_frontier()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        let page = self.inner.read(block)?;
        self.pages = self.pages.checked_add(1).ok_or(Error::Limit("count reads"))?;
        self.bytes = self.bytes.checked_add(page.bytes().len() as u64)
            .ok_or(Error::Limit("count copied bytes"))?;
        Ok(page)
    }
    fn extend(&mut self) -> Result<u32> {
        Err(Error::InvalidState)
    }
    fn commit(&mut self, _pages: &[&Page]) -> Result<()> {
        Err(Error::InvalidState)
    }
    fn interrupt(&mut self) -> Result<()> {
        self.inner.interrupt()
    }
    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
}

/// checks supported syntax without reading an index or making a visibility claim.
///
/// # Safety
/// bytes is an immutable initialized datum extent retained for this call.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_count_group_supported(bytes: *const u8, length: usize) -> bool {
    crate::compatibility::database();
    if bytes.is_null() || length > isize::MAX as usize {
        return matching::input(Err(Error::InvalidState));
    }
    // safety: the caller retains the one immutable detoasted allocation.
    let input = unsafe { std::slice::from_raw_parts(bytes, length) };
    let query = matching::input(Query::decode(input, QueryLimits::default()));
    grouped::supports_query(&query)
}

/// executes one serial mixed-visibility count; no Rust state escapes into a plan.
///
/// # Safety
/// C holds an open validated index and the shared liveness lock. context owns a
/// supported MVCC snapshot, slot and scratch context; query bytes remain live and
/// counters reserves COUNTERS aligned exclusive u64 values for this call.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_count_group_execute(
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
    // safety: C retains the immutable query independently of per-tuple scratch.
    let input = unsafe { std::slice::from_raw_parts(bytes, length) };
    let query = matching::input(Query::decode(input, QueryLimits::default()));
    let mut counter = Counter { context, query: &query, stats: [0; COUNTERS] };
    // safety: C retains retirement exclusion; this barrier additionally excludes
    // snapshot replacement and page reuse until VM/heap decisions are complete.
    matching::stored(unsafe {
        storage::with_reader(index, |store| {
            let mut measured = Metered { inner: store, pages: 0, bytes: 0 };
            grouped::scan_into(&mut measured, &query, matching::QUERY_MEMORY, &mut counter)?;
            counter.note(8, measured.pages)?;
            counter.note(9, measured.bytes)
        })
    });
    let count = matching::stored(
        counter.stats[3].checked_add(counter.stats[5])
            .and_then(|count| i64::try_from(count).ok())
            .ok_or(Error::Limit("SQL count")),
    );
    for (position, value) in counter.stats.iter().enumerate() {
        // safety: the caller reserves exactly COUNTERS aligned writable values.
        unsafe { counters.add(position).write(*value) };
    }
    count
}
