//! Recoverable, single-writer mutable storage independent of PostgreSQL pointers.
//! The adapter supplies atomic WAL batches, cancellation and a writer interlock.
//! Readers require an MVCC-equivalent host contract and never fetch payload pages.
//! G3 reclaims posting pages only after host reader quiescence.
//! Owner slots and dictionary identities are never reused.

mod compact;
pub mod document;
pub mod page;
mod reader;
mod vacuum;
mod writer;

pub use compact::{CompactStats, compact, recover as recover_compaction};
pub use reader::scan;
pub use vacuum::{VacuumStats, vacuum};
pub use writer::{initialize, insert};

use crate::error::{Error, Result};
use crate::identity::HeapLayout;
use page::{Page, PageKind};

/// Storage and reader-barrier boundaries for fault injection in disposable tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Stage {
    Allocation = 1,
    OwnerReserved = 2,
    FragmentStored = 3,
    PayloadReady = 4,
    TermLinked = 5,
    Published = 6,
    OwnerRemoved = 7,
    PageReclaimed = 8,
    SegmentStored = 9,
    ReplacementPublished = 10,
    SegmentReclaimed = 11,
    ReaderPinned = 12,
}

/// Host I/O contract; implementations must not retain page borrows.
///
/// Writers, VACUUM and recovery inspection hold one host writer interlock across
/// the complete operation. `commit` atomically persists at most MAX_WAL_PAGES
/// distinct page images before returning. `extend` reserves a never-used block;
/// a crash may leave that block all zero. Errors abort the whole host operation.
/// Readers hold a shared structural barrier from before their first page read
/// through their last candidate. Compaction takes its exclusive side before the
/// writer interlock; no operation upgrades a shared barrier. Recovery only frees
/// unreachable journal pages. Resource cleanup belongs to the host.
pub trait PageStore {
    fn layout(&self) -> HeapLayout;
    fn blocks(&mut self) -> Result<u32>;
    fn read(&mut self, block: u32) -> Result<Page>;
    fn extend(&mut self) -> Result<u32>;
    fn commit(&mut self, pages: &[&Page]) -> Result<()>;

    fn interrupt(&mut self) -> Result<()> {
        Ok(())
    }

    fn event(&mut self, _stage: Stage) -> Result<()> {
        Ok(())
    }
}

fn load<S: PageStore>(store: &mut S, block: u32, kind: PageKind) -> Result<Page> {
    let page = load_any(store, block)?;
    if page.kind() != kind {
        return Err(Error::InvalidState);
    }
    Ok(page)
}

fn load_any<S: PageStore>(store: &mut S, block: u32) -> Result<Page> {
    store.interrupt()?;
    if block >= store.blocks()? {
        return Err(Error::InvalidState);
    }
    let page = store.read(block)?;
    if page.block() != block {
        return Err(Error::InvalidState);
    }
    page.validate(store.layout())?;
    Ok(page)
}

fn allocate<S: PageStore>(store: &mut S) -> Result<u32> {
    store.interrupt()?;
    let expected = store.blocks()?;
    let block = store.extend()?;
    if block != expected || block == page::NO_BLOCK {
        return Err(Error::InvalidState);
    }
    store.event(Stage::Allocation)?;
    Ok(block)
}

fn following(page: &Page, tail: u32) -> Result<Option<u32>> {
    if page.block() == tail {
        return Ok(None);
    }
    let next = page.next()?;
    if next == page::NO_BLOCK || next > tail || next <= page.block() {
        return Err(Error::InvalidState);
    }
    Ok(Some(next))
}

// scans a captured dictionary chain; hashes route but never establish equality.
fn find_term<S: PageStore>(
    store: &mut S,
    meta: &Page,
    text: &str,
) -> Result<Option<(Page, page::TermRef)>> {
    let bucket = page::bucket_for(text);
    let (head, tail) = meta.bucket(bucket)?;
    if head == page::NO_BLOCK {
        return Ok(None);
    }
    let mut block = head;
    let mut found = None;
    loop {
        let page = load(store, block, PageKind::Dictionary)?;
        let mut reference = None;
        for entry in page.terms()? {
            let entry = entry?;
            if page::bucket_for(entry.term) != bucket {
                return Err(Error::InvalidState);
            }
            if entry.term == text {
                if reference.is_some() || found.is_some() {
                    return Err(Error::InvalidState);
                }
                reference = Some(entry.reference);
            }
        }
        let next = following(&page, tail)?;
        if let Some(reference) = reference {
            found = Some((page, reference));
        }
        match next {
            Some(next) => block = next,
            None => return Ok(found),
        }
    }
}

// posting blocks can be recycled; a captured relation size bounds every traversal.
fn posting_next(page: &Page, tail: u32, remaining: &mut u32) -> Result<Option<u32>> {
    *remaining = remaining.checked_sub(1).ok_or(Error::InvalidState)?;
    if page.block() == tail {
        return Ok(None);
    }
    let next = page.next()?;
    if next == page::NO_BLOCK || *remaining == 0 {
        return Err(Error::InvalidState);
    }
    Ok(Some(next))
}

fn load_posting<S: PageStore>(store: &mut S, block: u32, term: page::TermRef) -> Result<Page> {
    let page = load_any(store, block)?;
    if page.posting_term()? != term {
        return Err(Error::InvalidState);
    }
    Ok(page)
}
