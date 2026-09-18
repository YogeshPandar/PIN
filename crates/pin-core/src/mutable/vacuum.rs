//! Callback-authorized removal and recovery-safe fragment/orphan reclamation.
//! The host writer interlock excludes incomplete live writers during this pass.
//! Owner and dictionary identities remain allocated across posting compaction.

use super::page::{NO_BLOCK, OwnerChange, Page, PageKind};
use super::{PageStore, Stage, following, load, load_any};
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::identity::RootTid;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VacuumStats {
    pub live_documents: u64,
    pub removed_documents: u64,
    pub abandoned_documents: u64,
    pub reclaimed_pages: u32,
    pub free_pages: u32,
    pub pages: u32,
}

/// Removes each callback-approved owner once, then reclaims dead fragments.
///
/// # Errors
/// Rejects corruption or any host/callback failure. Partial completed batches are
/// durable and idempotent on the next pass; no published live owner is reclaimed.
/// Zero-page recovery checks every possible incoming reference before reuse.
pub fn vacuum<S: PageStore>(
    store: &mut S,
    mut removable: impl FnMut(RootTid) -> Result<bool>,
) -> Result<VacuumStats> {
    let recovered = super::recover_compaction(store)?;
    let mut meta = load(store, 0, PageKind::Meta)?;
    let mut stats = VacuumStats {
        reclaimed_pages: recovered,
        ..VacuumStats::default()
    };
    let (head, tail) = meta.owner_chain()?;
    if head != NO_BLOCK {
        let mut block = head;
        loop {
            let mut page = load(store, block, PageKind::Owners)?;
            let mut changed = false;
            for slot in 0..page.owner_count()? {
                let owner = page.owner(slot, store.layout())?;
                let reference = owner.reference;
                match owner.publication {
                    Publication::Published if owner.live => {
                        if removable(owner.root)? {
                            page.change_owner(reference, OwnerChange::Remove, store.layout())?;
                            stats.removed_documents += 1;
                            changed = true;
                        } else {
                            stats.live_documents += 1;
                        }
                    }
                    Publication::Allocated | Publication::FragmentsWritten => {
                        page.change_owner(reference, OwnerChange::Abandon, store.layout())?;
                        stats.abandoned_documents += 1;
                        changed = true;
                    }
                    _ => {}
                }
            }
            if changed {
                store.commit(&[&page])?;
                store.event(Stage::OwnerRemoved)?;
            }
            match following(&page, tail)? {
                Some(next) => block = next,
                None => break,
            }
        }
    }
    stats.pages = store.blocks()?;
    let mut has_zero = false;
    for block in 1..stats.pages {
        let page = load_any(store, block)?;
        match page.kind() {
            PageKind::Zero => has_zero = true,
            PageKind::Free => stats.free_pages += 1,
            PageKind::Fragment => {
                let (reference, _, _) = page.fragment_data()?;
                let owners = load(store, reference.page, PageKind::Owners)?;
                let owner = owners.owner(reference.slot, store.layout())?;
                if owner.reference != reference {
                    return Err(Error::InvalidState);
                }
                if owner.publication != Publication::Published || !owner.live {
                    reclaim(store, &mut meta, block, false)?;
                    stats.reclaimed_pages += 1;
                    stats.free_pages += 1;
                }
            }
            _ => {}
        }
    }
    if has_zero {
        for block in 1..stats.pages {
            if load_any(store, block)?.kind() == PageKind::Zero {
                check_unreferenced(store, block, stats.pages)?;
                reclaim(store, &mut meta, block, true)?;
                stats.reclaimed_pages += 1;
                stats.free_pages += 1;
            }
        }
    }
    Ok(stats)
}

fn reclaim<S: PageStore>(
    store: &mut S,
    meta: &mut Page,
    block: u32,
    zero: bool,
) -> Result<()> {
    let next = meta.free_head()?;
    let page = if zero {
        Page::free_from_zero(block, next)?
    } else {
        Page::free(block, next)?
    };
    meta.set_free_head(block)?;
    store.commit(&[meta, &page])?;
    store.event(Stage::PageReclaimed)
}

// rare allocation-orphan recovery uses bounded scratch rather than a relation-sized set.
fn check_unreferenced<S: PageStore>(store: &mut S, target: u32, pages: u32) -> Result<()> {
    for block in 0..pages {
        let page = load_any(store, block)?;
        if page.kind() == PageKind::Zero {
            continue;
        }
        if page.next()? == target {
            return Err(Error::InvalidState);
        }
        match page.kind() {
            PageKind::Meta => {
                if page
                    .rewrite_journal()?
                    .is_some_and(|journal| journal.head == target || journal.tail == target)
                {
                    return Err(Error::InvalidState);
                }
                let (head, tail) = page.owner_chain()?;
                if head == target || tail == target || page.free_head()? == target {
                    return Err(Error::InvalidState);
                }
                for bucket in 0..super::page::BUCKETS {
                    let (head, tail) = page.bucket(bucket)?;
                    if head == target || tail == target {
                        return Err(Error::InvalidState);
                    }
                }
            }
            PageKind::Owners => {
                for slot in 0..page.owner_count()? {
                    if page.owner(slot, store.layout())?.data_head == target {
                        return Err(Error::InvalidState);
                    }
                }
            }
            PageKind::Dictionary => {
                for term in page.terms()? {
                    let term = term?;
                    if term.head == target || term.tail == target || term.first.page == target {
                        return Err(Error::InvalidState);
                    }
                }
            }
            PageKind::Postings | PageKind::SealedPostings => {
                if page.posting_term()?.page == target {
                    return Err(Error::InvalidState);
                }
                for owner in page.posting_refs()? {
                    if owner?.page == target {
                        return Err(Error::InvalidState);
                    }
                }
            }
            PageKind::Fragment if page.fragment_data()?.0.page == target => {
                return Err(Error::InvalidState);
            }
            _ => {}
        }
    }
    Ok(())
}
