//! adapts owned Rust page images to PostgreSQL buffers and generic WAL.
//! the caller retains the relation lock; no page pointer escapes the C shim.
//! normal paths unlock explicitly; PostgreSQL abort cleanup handles ERROR/panic.
//! contracts and independent-review obligations: docs/g3-storage.md.

use crate::native;
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::pg_sys;
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::page::{CAPACITY, MAX_WAL_PAGES, Page, PageKind};
use pin_core::mutable::{PageStore, Stage};
use std::marker::PhantomData;

static ENABLE_EXACT_BITMAP: GucSetting<bool> = GucSetting::<bool>::new(true);

pub(crate) fn initialize() {
    GucRegistry::define_bool_guc(
        c"pin.enable_exact_bitmap",
        c"Use proven exact term and Boolean bitmap matches.",
        c"Off forces predicate rechecks; heap visibility checks always remain enabled.",
        &ENABLE_EXACT_BITMAP,
        GucContext::Suset,
        GucFlags::default(),
    );
}

pub(crate) struct PgStore<'rel> {
    index: pg_sys::Relation,
    layout: HeapLayout,
    extended: Option<u32>,
    writer: bool,
    relation: PhantomData<&'rel pg_sys::RelationData>,
}

impl PgStore<'_> {
    /// # Safety
    /// index must be a validated, open, locked relation on the backend main thread
    /// and remain so for every use of the returned store. no store escapes a callback.
    unsafe fn read_only(index: pg_sys::Relation) -> Result<Self> {
        Ok(Self {
            index,
            layout: HeapLayout::new(crate::abi::constant(9) as u16)
                .map_err(|_| Error::InvalidState)?,
            extended: None,
            writer: false,
            relation: PhantomData,
        })
    }
}

/// executes one write operation without holding a buffer lock across Rust work.
///
/// # Safety
/// index is validated and remains open/locked throughout the guarded callback.
/// a host ERROR must propagate to PostgreSQL, never be swallowed as ordinary success.
pub(crate) unsafe fn with_writer<T>(
    index: pg_sys::Relation,
    operation: impl FnOnce(&mut PgStore<'_>) -> Result<T>,
) -> Result<T> {
    // safety: the caller retains the live index relation through this operation.
    let mut store = unsafe { PgStore::read_only(index)? };
    // safety: this trivial call acquires a resource-owned heavyweight page lock.
    unsafe { native::call(|| native::pin_writer_lock(index)) };
    store.writer = true;
    let result = operation(&mut store);
    // safety: exactly one acquisition occurred; ERROR paths release via abort cleanup.
    unsafe { native::call(|| native::pin_writer_unlock(index)) };
    result
}

/// holds the structural read barrier only while producing bitmap candidates.
///
/// # Safety
/// index is validated and remains open/locked throughout the guarded callback.
/// the result must not retain page references or use the store after this call.
pub(crate) unsafe fn with_reader<T>(
    index: pg_sys::Relation,
    operation: impl FnOnce(&mut PgStore<'_>) -> Result<T>,
) -> Result<T> {
    // safety: the caller retains its validated relation throughout this operation.
    let mut store = unsafe { PgStore::read_only(index)? };
    // safety: a transaction-owned shared page lock prevents structural reclamation.
    unsafe { native::call(|| native::pin_structure_lock(index, false)) };
    #[cfg(feature = "test-hooks")]
    crate::test_hooks::storage_event(Stage::ReaderPinned);
    let result = operation(&mut store);
    // safety: one shared acquisition precedes this call; ERROR uses abort cleanup.
    unsafe { native::call(|| native::pin_structure_unlock(index, false)) };
    result
}

/// reads under a structural barrier already held by the host scan.
///
/// # Safety
/// index is validated and remains open/locked for the operation. the caller
/// must hold the shared structural barrier until all captured work is consumed.
pub(crate) unsafe fn with_locked_reader<T>(
    index: pg_sys::Relation,
    operation: impl FnOnce(&mut PgStore<'_>) -> Result<T>,
) -> Result<T> {
    // safety: the caller retains the live relation and structural barrier.
    let mut store = unsafe { PgStore::read_only(index)? };
    operation(&mut store)
}

/// excludes readers before writers, so no captured posting page can be recycled.
///
/// # Safety
/// index has the same guarded lifetime as with_writer. no lock upgrade is allowed:
/// the caller holds neither the structural barrier nor the writer interlock.
pub(crate) unsafe fn with_maintenance<T>(
    index: pg_sys::Relation,
    operation: impl FnOnce(&mut PgStore<'_>) -> Result<T>,
) -> Result<T> {
    // safety: the caller owns neither interlock; relation lifetime covers both.
    unsafe { native::call(|| native::pin_structure_lock(index, true)) };
    // safety: structural exclusion precedes writer exclusion in every maintenance path.
    let result = unsafe { with_writer(index, operation) };
    // safety: one exclusive acquisition precedes this call; ERROR uses abort cleanup.
    unsafe { native::call(|| native::pin_structure_unlock(index, true)) };
    result
}

impl PageStore for PgStore<'_> {
    fn frontier_anchors(&self) -> bool {
        crate::grouped::frontier_anchors_enabled()
    }

    fn owner_frontier(&self) -> bool {
        crate::grouped::owner_frontier_enabled()
    }

    fn layout(&self) -> HeapLayout {
        self.layout
    }

    fn blocks(&mut self) -> Result<u32> {
        let index = self.index;
        // safety: the store retains its caller's live relation for this trivial C call.
        Ok(unsafe { native::call(|| native::pin_storage_blocks(index)) })
    }

    fn read(&mut self, block: u32) -> Result<Page> {
        let index = self.index;
        Page::read_with(block, |output| {
            let pointer = output.as_mut_ptr();
            let capacity = output.len() as u32;
            // safety: one exclusive output slice covers capacity bytes; C copies under
            // a shared buffer lock and retains no pointer after releasing the buffer.
            Ok(
                unsafe {
                    native::call(|| native::pin_storage_read(index, block, pointer, capacity))
                } as usize,
            )
        })
    }

    fn extend(&mut self) -> Result<u32> {
        if !self.writer || self.extended.is_some() {
            return Err(Error::InvalidState);
        }
        let index = self.index;
        // safety: the writer interlock serializes allocation; C releases its buffer pin.
        let block = unsafe { native::call(|| native::pin_storage_extend(index)) };
        self.extended = Some(block);
        Ok(block)
    }

    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        if !self.writer || pages.is_empty() || pages.len() > MAX_WAL_PAGES {
            return Err(Error::InvalidState);
        }
        let count = pages.len();
        let mut order = [0, 1, 2];
        order[..count].sort_unstable_by_key(|&position| pages[position].block());
        let mut blocks = [0u32; MAX_WAL_PAGES];
        let mut pointers = [std::ptr::null(); MAX_WAL_PAGES];
        let mut lengths = [0u32; MAX_WAL_PAGES];
        let mut full = [false; MAX_WAL_PAGES];
        for (slot, &position) in order[..count].iter().enumerate() {
            let page = pages[position];
            page.validate(self.layout)?;
            if page.bytes().is_empty()
                || page.bytes().len() > CAPACITY
                || (slot > 0 && blocks[slot - 1] == page.block())
            {
                return Err(Error::InvalidState);
            }
            blocks[slot] = page.block();
            pointers[slot] = page.bytes().as_ptr();
            lengths[slot] = page.bytes().len() as u32;
            // only new storage needs a full image; existing standard pages use deltas.
            full[slot] = self.extended == Some(page.block()) || page.initializes_storage();
        }
        let index = self.index;
        let count_u32 = count as u32;
        let blocks_ptr = blocks.as_ptr();
        let pointers_ptr = pointers.as_ptr();
        let lengths_ptr = lengths.as_ptr();
        let full_ptr = full.as_ptr();
        // safety: distinct ascending pages and all extents are checked before locks.
        // C reads these private immutable images synchronously and mutates only WAL copies.
        unsafe {
            native::call(|| {
                native::pin_storage_commit(
                    index,
                    count_u32,
                    blocks_ptr,
                    pointers_ptr,
                    lengths_ptr,
                    full_ptr,
                )
            })
        };
        if self
            .extended
            .is_some_and(|block| blocks[..count].contains(&block))
        {
            self.extended = None;
        }
        Ok(())
    }

    fn remove_owners(&mut self, page: &Page) -> Result<()> {
        if !self.writer || page.kind() != PageKind::Owners || self.extended.is_some() {
            return Err(Error::InvalidState);
        }
        page.validate(self.layout)?;
        let index = self.index;
        let block = page.block();
        let bytes = page.bytes().as_ptr();
        let length = page.bytes().len() as u32;
        // safety: the writer interlock covers the private image's read and mutation.
        // C takes cleanup permission before publishing removal through generic WAL.
        unsafe { native::call(|| native::pin_storage_remove_owners(index, block, bytes, length)) };
        Ok(())
    }

    fn interrupt(&mut self) -> Result<()> {
        interrupt();
        Ok(())
    }

    fn event(&mut self, stage: Stage) -> Result<()> {
        #[cfg(feature = "test-hooks")]
        crate::test_hooks::storage_event(stage);
        #[cfg(not(feature = "test-hooks"))]
        let _ = stage;
        Ok(())
    }
}

pub(crate) fn interrupt() {
    // safety: guarded backend-only callers; the closure owns nothing and only enters C.
    unsafe { native::call(|| native::pin_storage_interrupt()) };
}

/// # Safety
/// tid is a core-provided live ItemPointer; the caller is a guarded AM callback.
pub(crate) unsafe fn root(tid: pg_sys::ItemPointer) -> Result<RootTid> {
    let mut block = 0;
    let mut offset = 0;
    let block_ptr = &mut block as *mut u32;
    let offset_ptr = &mut offset as *mut u16;
    // safety: C validates the item pointer and fills two distinct initialized scalars.
    unsafe { native::call(|| native::pin_root_coordinates(tid, block_ptr, offset_ptr)) };
    let layout =
        HeapLayout::new(crate::abi::constant(9) as u16).map_err(|_| Error::InvalidState)?;
    RootTid::new(block, offset, layout).map_err(|_| Error::InvalidState)
}

pub(crate) struct BitmapSink {
    bitmap: *mut pg_sys::TIDBitmap,
    blocks: [u32; 256],
    offsets: [u16; 256],
    len: usize,
    recheck: bool,
    force_recheck: bool,
}

impl BitmapSink {
    /// # Safety
    /// bitmap remains caller-owned and writable until the sink is discarded.
    pub(crate) unsafe fn new(bitmap: *mut pg_sys::TIDBitmap) -> Self {
        Self {
            bitmap,
            blocks: [0; 256],
            offsets: [0; 256],
            len: 0,
            recheck: true,
            force_recheck: !ENABLE_EXACT_BITMAP.get(),
        }
    }

    pub(crate) fn push(&mut self, root: RootTid, recheck: bool) -> Result<()> {
        let recheck = recheck || self.force_recheck;
        if self.recheck != recheck {
            self.flush();
            self.recheck = recheck;
        }
        self.blocks[self.len] = root.block();
        self.offsets[self.len] = root.offset();
        self.len += 1;
        if self.len == self.blocks.len() {
            self.flush();
        }
        Ok(())
    }

    pub(crate) fn flush(&mut self) {
        if self.len == 0 {
            return;
        }
        let bitmap = self.bitmap;
        let count = self.len as u32;
        let blocks = self.blocks.as_ptr();
        let offsets = self.offsets.as_ptr();
        let recheck = self.recheck;
        // safety: count initialized coordinates fit both arrays; c copies them
        // synchronously. the executed plan and all scan keys determine recheck.
        unsafe { native::call(|| native::pin_bitmap_add(bitmap, count, blocks, offsets, recheck)) };
        self.len = 0;
    }
}
