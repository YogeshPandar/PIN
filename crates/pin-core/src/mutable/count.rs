//! Duplicate-free owner streaming for the guarded direct-count adapter.
//! Sealed membership is not visibility: the host rereads the canonical owner
//! under protection before consulting the VM, or fetches and rechecks the heap.
//! Contracts: docs/g5-counts.md; PostgreSQL 18 index-locking and storage-vm.

use super::page::{NO_BLOCK, OwnerRef, PageKind};
use super::{PageStore, find_term, following, load, load_posting, posting_next};
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::query::{Kind, Query};

/// An incarnation-qualified candidate, never a snapshot-visible row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CountCandidate {
    pub owner: OwnerRef,
    /// True only for an exact single-term hit in a sealed posting page.
    /// The host must still validate publication, liveness and VM ordering.
    pub sealed_term: bool,
}

/// Streams each possible matching owner once with constant-sized private state.
/// The host holds the shared structural barrier throughout this call. Only a
/// single term uses postings; other expressions use the non-null universe and
/// require a full heap predicate recheck. No lossy union or result set is built.
///
/// # Errors
/// Rejects malformed chains, duplicate/out-of-order term owners and host errors.
/// The returned value is candidate accounting, not an exact SQL count. Host
/// errors abort the operation; no partial count may be exposed to the caller.
pub fn scan_count<S: PageStore>(
    store: &mut S,
    query: &Query,
    mut emit: impl FnMut(CountCandidate) -> Result<()>,
) -> Result<u64> {
    let kind = &query.nodes[query.root].kind;
    if matches!(kind, Kind::None) {
        return Ok(0);
    }
    let meta = load(store, 0, PageKind::Meta)?;
    let mut count = 0u64;
    let mut send = |owner, sealed_term| -> Result<()> {
        emit(CountCandidate { owner, sealed_term })?;
        count = count.checked_add(1).ok_or(Error::Limit("count candidates"))?;
        Ok(())
    };
    if let Kind::Term(term) = kind {
        let Some((dictionary, reference)) = find_term(store, &meta, term)? else {
            return Ok(0);
        };
        let entry = dictionary.term(reference)?;
        let mut previous = entry.first;
        // the inline dictionary owner has no sealed-source certificate.
        send(previous, false)?;
        let (mut block, tail) = (entry.head, entry.tail);
        if block != NO_BLOCK {
            let mut remaining = store.blocks()?;
            loop {
                let page = load_posting(store, block, reference)?;
                let sealed = page.kind() == PageKind::SealedPostings;
                for owner in page.posting_refs()? {
                    let owner = owner?;
                    if (owner.page, owner.slot) <= (previous.page, previous.slot)
                        || owner.incarnation <= previous.incarnation
                    {
                        return Err(Error::InvalidState);
                    }
                    send(owner, sealed)?;
                    previous = owner;
                }
                match posting_next(&page, tail, &mut remaining)? {
                    Some(next) => block = next,
                    None => break,
                }
            }
        }
    } else {
        let (mut block, tail) = meta.owner_chain()?;
        if block != NO_BLOCK {
            loop {
                let page = load(store, block, PageKind::Owners)?;
                for slot in 0..page.owner_count()? {
                    let owner = page.owner(slot, store.layout())?;
                    // dead/unpublished owners cannot become visible in this snapshot.
                    if owner.live && owner.publication == Publication::Published {
                        send(owner.reference, false)?;
                    }
                }
                match following(&page, tail)? {
                    Some(next) => block = next,
                    None => break,
                }
            }
        }
    }
    Ok(count)
}
