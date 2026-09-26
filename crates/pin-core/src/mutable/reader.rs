//! Bounded candidate streaming with captured chain tails and one owner-page cache.
//! This conservative reader never follows document payloads. The exact phrase
//! reader in query.rs follows fragments under the same structural barrier.
//! The host must use MVCC and recheck every root emitted by this reader.

use super::page::{NO_BLOCK, OwnerRef, Page, PageKind};
use super::{PageStore, find_term, following, load, load_posting, posting_next};
use crate::candidate::CandidatePlan;
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::identity::RootTid;

/// Streams a conservative cover; returned accounting is not a visible SQL count.
/// The host retains the shared structural barrier for this entire call.
///
/// # Errors
/// Rejects corruption, missing owners, incarnation mismatches and host failures.
/// The caller owns duplicate elimination, bitmap lossification and heap rechecks.
pub fn scan<S: PageStore>(
    store: &mut S,
    plan: &CandidatePlan<'_>,
    mut emit: impl FnMut(RootTid) -> Result<()>,
) -> Result<u64> {
    let meta = load(store, 0, PageKind::Meta)?;
    let mut count = 0u64;
    match plan {
        CandidatePlan::Empty => {}
        CandidatePlan::Universe => {
            let (head, tail) = meta.owner_chain()?;
            if head == NO_BLOCK {
                return Ok(0);
            }
            let mut block = head;
            loop {
                let page = load(store, block, PageKind::Owners)?;
                for slot in 0..page.owner_count()? {
                    let owner = page.owner(slot, store.layout())?;
                    if owner.publication == Publication::Published && owner.live {
                        emit(owner.root)?;
                        count = count
                            .checked_add(1)
                            .ok_or(Error::Limit("candidate count"))?;
                    }
                }
                match following(&page, tail)? {
                    Some(next) => block = next,
                    None => break,
                }
            }
        }
        CandidatePlan::Terms(terms) => {
            let mut cache: Option<Page> = None;
            for term in terms {
                let Some((dictionary, reference)) = find_term(store, &meta, term)? else {
                    continue;
                };
                let entry = dictionary.term(reference)?;
                if let Some(root) = resolve(store, &mut cache, entry.first)? {
                    emit(root)?;
                    count = count
                        .checked_add(1)
                        .ok_or(Error::Limit("candidate count"))?;
                }
                let (head, tail) = (entry.head, entry.tail);
                if head == NO_BLOCK {
                    continue;
                }
                let mut block = head;
                let mut remaining = store.blocks()?;
                loop {
                    let page = load_posting(store, block, reference)?;
                    for owner in page.posting_refs()? {
                        if let Some(root) = resolve(store, &mut cache, owner?)? {
                            emit(root)?;
                            count = count
                                .checked_add(1)
                                .ok_or(Error::Limit("candidate count"))?;
                        }
                    }
                    match posting_next(&page, tail, &mut remaining)? {
                        Some(next) => block = next,
                        None => break,
                    }
                }
            }
        }
    }
    Ok(count)
}

pub(super) fn resolve<S: PageStore>(
    store: &mut S,
    cache: &mut Option<Page>,
    reference: OwnerRef,
) -> Result<Option<RootTid>> {
    // a concurrent append may outgrow a private owner-page copy.
    let reload = match cache.as_ref() {
        Some(page) if page.block() == reference.page => reference.slot >= page.owner_count()?,
        _ => true,
    };
    if reload {
        *cache = Some(load(store, reference.page, PageKind::Owners)?);
    }
    let page = cache.as_ref().ok_or(Error::InvalidState)?;
    let owner = page.owner(reference.slot, store.layout())?;
    if owner.reference != reference {
        return Err(Error::InvalidState);
    }
    Ok((owner.live && owner.publication == Publication::Published).then_some(owner.root))
}
