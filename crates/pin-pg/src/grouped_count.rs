//! opt-in page-popcount counts under a stable owner-generation interlock.
//! no heap visibility, PostgreSQL pointers or persistent caches enter pin-core.
//! proof and unresolved native gates: docs/g9-grouped-count.md, api-evidence count03.

use crate::{count::COUNTERS, matching, native, storage};
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::{pg_guard, pg_sys};
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::grouped::{self, ExactSink};
use pin_core::mutable::page::Page;
use pin_core::mutable::{PageStore, Stage};
use pin_core::query::{Query, QueryLimits};
use std::ffi::c_void;

static ENABLE_GROUPED_COUNT: GucSetting<bool> = GucSetting::<bool>::new(false);

unsafe extern "C-unwind" {
    fn pin_count_generation_try_lock(context: *mut c_void) -> bool;
    fn pin_count_generation_unlock(context: *mut c_void);
    fn pin_count_all_visible(context: *mut c_void, block: u32) -> bool;
    fn pin_count_fetch_visible(context: *mut c_void, block: u32, offset: u16) -> bool;
    fn pin_count_clear(context: *mut c_void);
}

pub(crate) fn initialize() {
    GucRegistry::define_bool_guc(
        c"pin.enable_grouped_count",
        c"Enable experimental grouped page-popcount counts.",
        c"Requires count fastpath; writer contention retains the core aggregate.",
        &ENABLE_GROUPED_COUNT,
        GucContext::Suset,
        GucFlags::default(),
    );
}

// instrumentation applies only to this opt-in consumer, not the bitmap read path.
struct ObservedStore<'a, S> {
    inner: &'a mut S,
    reads: u64,
    bytes: u64,
}

impl<S: PageStore> PageStore for ObservedStore<'_, S> {
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
        self.reads = self
            .reads
            .checked_add(1)
            .ok_or(Error::Limit("count page reads"))?;
        self.bytes = self
            .bytes
            .checked_add(page.bytes().len() as u64)
            .ok_or(Error::Limit("count page bytes"))?;
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

struct PageCounter {
    context: *mut c_void,
    stats: [u64; COUNTERS],
}

impl PageCounter {
    fn note(&mut self, counter: usize, amount: u64) -> Result<()> {
        self.stats[counter] = self.stats[counter]
            .checked_add(amount)
            .ok_or(Error::Limit("count instrumentation"))?;
        Ok(())
    }

    fn all_visible(&mut self, block: u32) -> Result<bool> {
        storage::interrupt();
        self.note(3, 1)?;
        #[cfg(feature = "test-hooks")]
        crate::test_hooks::storage_event(Stage::CountBeforeVisibility);
        let context = self.context;
        // safety: the generation interlock covers every source read and this VM test.
        // no VM result is retained across result pages, roots, rescans or executions.
        let visible = unsafe { native::call(|| pin_count_all_visible(context, block)) };
        #[cfg(feature = "test-hooks")]
        crate::test_hooks::storage_event(Stage::CountAfterVisibility);
        Ok(visible)
    }

    fn heap(&mut self, root: RootTid) -> Result<()> {
        self.note(5, 1)?;
        let context = self.context;
        let block = root.block();
        let offset = root.offset();
        // safety: the protected exact root cannot retire or be reused; C follows
        // its HOT chain under the active MVCC snapshot without borrowing tuple data.
        let visible = unsafe { native::call(|| pin_count_fetch_visible(context, block, offset)) };
        // safety: no tuple borrow escapes fetch_visible; the initialized slot is live.
        unsafe { native::call(|| pin_count_clear(context)) };
        if visible {
            self.note(6, 1)?;
        }
        Ok(())
    }
}

impl ExactSink for PageCounter {
    fn work(&mut self, stats: pin_core::grouped::QueryStats) -> Result<()> {
        self.note(14, u64::from(stats.term_payloads))?;
        self.note(15, u64::from(stats.term_bytes))?;
        self.note(16, u64::from(stats.candidate_pages))?;
        self.note(17, u64::from(stats.live_pages))
    }

    fn tid(&mut self, root: RootTid) -> Result<()> {
        self.note(0, 1)?;
        self.note(10, 1)?;
        if self.all_visible(root.block())? {
            self.note(4, 1)
        } else {
            self.heap(root)
        }
    }

    fn page(&mut self, layout: HeapLayout, block: u32, offsets: &[u64; 8]) -> Result<()> {
        // the core has validated both term and liveness masks for this heap layout.
        let count: u64 = offsets.iter().map(|word| u64::from(word.count_ones())).sum();
        self.note(0, count)?;
        self.note(8, 1)?;
        self.note(9, count)?;
        if self.all_visible(block)? {
            return self.note(4, count);
        }
        for (word, &value) in offsets.iter().enumerate() {
            let mut pending = value;
            while pending != 0 {
                let bit = word * 64 + pending.trailing_zeros() as usize;
                pending &= pending - 1;
                self.heap(
                    RootTid::new(block, (bit + 1) as u16, layout)
                        .map_err(|_| Error::InvalidState)?,
                )?;
            }
        }
        Ok(())
    }
}

/// reads the backend-local runtime gate without decoding a query again.
#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pin_count_grouped_enabled() -> bool {
    ENABLE_GROUPED_COUNT.get()
}

/// returns syntactic and runtime-GUC eligibility, not a storage/visibility proof.
///
/// # Safety
/// bytes contains one initialized immutable query allocation for this call.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_count_grouped_eligible(bytes: *const u8, length: usize) -> bool {
    if !ENABLE_GROUPED_COUNT.get() {
        return false;
    }
    crate::compatibility::database();
    if bytes.is_null() || length > isize::MAX as usize {
        return matching::input(Err(Error::InvalidState));
    }
    // safety: the planner/executor retains the detoasted query allocation throughout.
    let input = unsafe { std::slice::from_raw_parts(bytes, length) };
    let query = matching::input(Query::decode(input, QueryLimits::default()));
    grouped::supports_exact(&query)
}

/// returns false without publishing output when the protected fast path is unavailable.
/// corruption and host errors propagate instead of being hidden by fallback.
///
/// # Safety
/// index and context are validated live resources for an MVCC count. bytes is one
/// immutable query allocation; result and COUNTERS aligned u64s are exclusively
/// writable and live for this synchronous invocation. No native state escapes.
#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pin_count_grouped_execute(
    index: pg_sys::Relation,
    context: *mut c_void,
    bytes: *const u8,
    length: usize,
    memory_bytes: usize,
    result: *mut i64,
    counters: *mut u64,
) -> bool {
    crate::compatibility::database();
    if index.is_null()
        || context.is_null()
        || bytes.is_null()
        || result.is_null()
        || counters.is_null()
        || length > isize::MAX as usize
    {
        matching::stored::<()>(Err(Error::InvalidState));
    }
    if !ENABLE_GROUPED_COUNT.get() {
        return false;
    }
    // safety: C retains the immutable query outside its resettable tuple context.
    let input = unsafe { std::slice::from_raw_parts(bytes, length) };
    let query = matching::input(Query::decode(input, QueryLimits::default()));
    let mut counter = PageCounter {
        context,
        stats: [0; COUNTERS],
    };
    // safety: C retains both relations and the supported snapshot. The nested
    // lock order is structural share, conditional writer share, then buffer reads.
    let completed = matching::stored(unsafe {
        storage::with_reader(index, |store| {
            if !native::call(|| pin_count_generation_try_lock(context)) {
                return Ok(None);
            }
            #[cfg(feature = "test-hooks")]
            crate::test_hooks::storage_event(Stage::GroupCountProtected);
            let mut observed = ObservedStore {
                inner: store,
                reads: 0,
                bytes: 0,
            };
            let scanned = grouped::scan_exact(
                &mut observed,
                &query,
                memory_bytes.min(matching::QUERY_MEMORY),
                &mut counter,
            );
            // release even on a pure-core error; PostgreSQL ERROR uses abort cleanup.
            native::call(|| pin_count_generation_unlock(context));
            counter.stats[12] = observed.reads;
            counter.stats[13] = observed.bytes;
            scanned
        })
    });
    let Some(candidates) = completed else {
        return false;
    };
    if candidates != counter.stats[0] {
        matching::stored::<()>(Err(Error::InvalidState));
    }
    counter.stats[11] = 1;
    let visible = matching::stored(
        counter.stats[4]
            .checked_add(counter.stats[6])
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(Error::Limit("SQL count")),
    );
    // safety: the C caller reserves one aligned i64 plus COUNTERS aligned u64s.
    // no output is published until the whole protected scan has succeeded.
    unsafe {
        result.write(visible);
        for (position, value) in counter.stats.iter().enumerate() {
            counters.add(position).write(*value);
        }
    }
    true
}
