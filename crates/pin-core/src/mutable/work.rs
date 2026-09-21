//! Pointer-free, page-sized work for serial and PostgreSQL parallel readers.
//! The host snapshots the words under its lock, prepares outside that lock, and
//! publishes the successor with compare-and-replace before consuming the batch.
//! A failed participant aborts the query; claimed batches must not be retried.
//! Contracts: PostgreSQL 18 index-functions and docs/g7-worker-protocol.md.

use super::page::{NO_BLOCK, OwnerRef, Page, PageKind, TermRef};
use super::{CountCandidate, PageStore, find_term, following, load, load_posting, posting_next};
use crate::candidate::CandidatePlan;
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::identity::{HeapLayout, Incarnation};
use crate::query::Query;

/// Number of scalar words in the transient host coordination protocol.
pub const WORK_WORDS: usize = 11;

/// A captured chain position, not a persistent format or a visibility proof.
/// Words contain only checked integers; synchronization belongs to the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkState([u64; WORK_WORDS]);

/// One privately copied page or inline reference, consumed only by its claimant.
pub struct WorkBatch {
    page: Option<Page>,
    inline: Option<OwnerRef>,
    sealed_term: bool,
}

impl WorkState {
    pub const DONE: Self = Self([0; WORK_WORDS]);

    /// Captures a duplicate-free cover while the host holds its reader barrier.
    /// A one-term necessary condition uses postings. Wider unions use canonical
    /// owners, avoiding a shared result set and cross-worker duplicate removal.
    /// Every emitted root still requires the host's visibility and SQL checks.
    ///
    /// # Errors
    /// Rejects corrupt metadata, failed I/O, and a cover exceeding `memory_bytes`.
    /// No page borrow or query reference survives this call.
    pub fn capture<S: PageStore>(
        store: &mut S,
        query: &Query,
        memory_bytes: usize,
    ) -> Result<Self> {
        let cover = CandidatePlan::build(query, memory_bytes)?;
        if cover == CandidatePlan::Empty {
            return Ok(Self::DONE);
        }
        let meta = load(store, 0, PageKind::Meta)?;
        let limit = u64::from(store.blocks()?);
        let mut words = [0; WORK_WORDS];
        words[3] = limit;
        words[9] = limit;
        if let CandidatePlan::Terms(terms) = &cover
            && let [term] = terms.as_slice()
        {
            let Some((dictionary, reference)) = find_term(store, &meta, term)? else {
                return Ok(Self::DONE);
            };
            let entry = dictionary.term(reference)?;
            words[0] = 1;
            words[1] = u64::from(entry.head);
            words[2] = u64::from(entry.tail);
            words[4] = u64::from(reference.page);
            words[5] = u64::from(reference.offset);
            words[6] = u64::from(entry.first.page);
            words[7] = u64::from(entry.first.slot);
            words[8] = entry.first.incarnation.get();
            words[10] = u64::from(query.is_single_term());
        } else {
            let (head, tail) = meta.owner_chain()?;
            if head == NO_BLOCK {
                return Ok(Self::DONE);
            }
            words[0] = 3;
            words[1] = u64::from(head);
            words[2] = u64::from(tail);
        }
        Self::from_words(words)
    }

    pub const fn words(self) -> [u64; WORK_WORDS] {
        self.0
    }

    /// Validates a private copy of host coordination words without allocation.
    ///
    /// # Errors
    /// Rejects unknown kinds, truncation, invalid identities and chain bounds.
    /// The host must separately guarantee atomic snapshots and a live barrier.
    pub fn from_words(words: [u64; WORK_WORDS]) -> Result<Self> {
        let state = Self(words);
        if words[0] == 0 {
            return if state == Self::DONE {
                Ok(state)
            } else {
                Err(Error::InvalidState)
            };
        }
        if words[0] > 3
            || words[9] == 0
            || words[9] >= u64::from(NO_BLOCK)
            || words[3] == 0
            || words[3] > words[9]
            || words[10] > 1
        {
            return Err(Error::InvalidState);
        }
        let valid_block = |word: u64| word > 0 && word < words[9];
        let empty = words[1] == u64::from(NO_BLOCK) && words[2] == u64::from(NO_BLOCK);
        if !(valid_block(words[1]) && valid_block(words[2])) && !(words[0] == 1 && empty) {
            return Err(Error::InvalidState);
        }
        if words[0] == 3 {
            if words[1] > words[2] || words[4..9].iter().any(|value| *value != 0) || words[10] != 0
            {
                return Err(Error::InvalidState);
            }
        } else {
            if !valid_block(words[4])
                || !valid_block(words[6])
                || words[5] > u64::from(u16::MAX)
                || words[7] > u64::from(u16::MAX)
            {
                return Err(Error::InvalidState);
            }
            state.previous()?;
        }
        Ok(state)
    }

    fn previous(self) -> Result<OwnerRef> {
        Ok(OwnerRef {
            page: u32::try_from(self.0[6]).map_err(|_| Error::InvalidState)?,
            slot: u16::try_from(self.0[7]).map_err(|_| Error::InvalidState)?,
            incarnation: Incarnation::new(self.0[8]).map_err(|_| Error::InvalidState)?,
        })
    }

    /// Prepares one batch without mutating shared state or allocating a result set.
    /// The host must atomically replace `self` with the returned successor before
    /// emitting that batch. On a lost race, discard it and take a fresh snapshot.
    /// No buffer read, decoder, callback or error handler may run under that lock.
    ///
    /// # Errors
    /// Rejects invalid/cyclic chains, duplicate owners, incarnation regression,
    /// changed term identity and host failures. Errors invalidate the whole scan.
    pub fn prepare<S: PageStore>(self, store: &mut S) -> Result<Option<(Self, WorkBatch)>> {
        if self == Self::DONE {
            return Ok(None);
        }
        let mut next = self;
        if self.0[0] == 1 {
            next.0[0] = 2;
            if self.0[1] == u64::from(NO_BLOCK) {
                next = Self::DONE;
            }
            return Ok(Some((
                next,
                WorkBatch {
                    page: None,
                    inline: Some(self.previous()?),
                    sealed_term: false,
                },
            )));
        }
        let block = u32::try_from(self.0[1]).map_err(|_| Error::InvalidState)?;
        let tail = u32::try_from(self.0[2]).map_err(|_| Error::InvalidState)?;
        let mut remaining = u32::try_from(self.0[3]).map_err(|_| Error::InvalidState)?;
        let page;
        let following_block;
        if self.0[0] == 3 {
            page = load(store, block, PageKind::Owners)?;
            following_block = following(&page, tail)?;
            remaining = remaining.checked_sub(1).ok_or(Error::InvalidState)?;
        } else {
            let term = TermRef {
                page: u32::try_from(self.0[4]).map_err(|_| Error::InvalidState)?,
                offset: u16::try_from(self.0[5]).map_err(|_| Error::InvalidState)?,
            };
            page = load_posting(store, block, term)?;
            let mut previous = self.previous()?;
            for owner in page.posting_refs()? {
                let owner = owner?;
                if (owner.page, owner.slot) <= (previous.page, previous.slot)
                    || owner.incarnation <= previous.incarnation
                {
                    return Err(Error::InvalidState);
                }
                previous = owner;
            }
            next.0[6] = u64::from(previous.page);
            next.0[7] = u64::from(previous.slot);
            next.0[8] = previous.incarnation.get();
            following_block = posting_next(&page, tail, &mut remaining)?;
        }
        if let Some(block) = following_block {
            next.0[1] = u64::from(block);
            next.0[3] = u64::from(remaining);
            next = Self::from_words(next.0)?;
        } else {
            next = Self::DONE;
        }
        let sealed_term = self.0[10] == 1 && page.kind() == PageKind::SealedPostings;
        Ok(Some((
            next,
            WorkBatch {
                page: Some(page),
                inline: None,
                sealed_term,
            },
        )))
    }
}

impl WorkBatch {
    /// Emits only this claimed batch; candidates are not visibility-certified.
    ///
    /// # Errors
    /// Propagates corruption or callback failure without a partial-result promise.
    /// The host aborts the operation rather than returning a partial SQL result.
    pub fn for_each(
        &self,
        layout: HeapLayout,
        mut emit: impl FnMut(CountCandidate) -> Result<()>,
    ) -> Result<()> {
        if let Some(owner) = self.inline {
            return emit(CountCandidate {
                owner,
                sealed_term: false,
            });
        }
        let page = self.page.as_ref().ok_or(Error::InvalidState)?;
        if page.kind() == PageKind::Owners {
            for slot in 0..page.owner_count()? {
                let owner = page.owner(slot, layout)?;
                if owner.live && owner.publication == Publication::Published {
                    emit(CountCandidate {
                        owner: owner.reference,
                        sealed_term: false,
                    })?;
                }
            }
        } else {
            for owner in page.posting_refs()? {
                emit(CountCandidate {
                    owner: owner?,
                    sealed_term: self.sealed_term,
                })?;
            }
        }
        Ok(())
    }
}
