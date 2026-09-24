//! lazy term-addressable suffixes over existing canonical posting chains.
//! contracts: docs/issue-14-boolean-frontier.md and api evidence BF01.

use super::page::{NO_BLOCK, OwnedPostings, OwnerRef, Page, PageKind, Term, TermRef};
use super::{PageStore, load_posting, posting_next};
use crate::error::{Error, Result};
use std::cmp::Ordering;

// dictionary values are copied while its checked private page is alive.
#[derive(Clone, Copy)]
pub(super) struct SuffixTerm {
    reference: TermRef,
    first: OwnerRef,
    head: u32,
    tail: u32,
}

impl From<Term<'_>> for SuffixTerm {
    fn from(term: Term<'_>) -> Self {
        Self {
            reference: term.reference,
            first: term.first,
            head: term.head,
            tail: term.tail,
        }
    }
}

pub(super) struct SuffixCursor {
    entry: Option<SuffixTerm>,
    after: Option<OwnerRef>,
    current: Option<OwnerRef>,
    previous: Option<OwnerRef>,
    block: u32,
    remaining: u32,
    page: Option<OwnedPostings>,
    opened: bool,
}

impl SuffixCursor {
    pub(super) fn new(entry: Option<SuffixTerm>, after: Option<OwnerRef>) -> Self {
        Self {
            entry,
            after,
            current: None,
            previous: None,
            block: NO_BLOCK,
            remaining: 0,
            page: None,
            opened: false,
        }
    }

    fn open<S: PageStore>(&mut self, store: &mut S) -> Result<()> {
        self.opened = true;
        let Some(entry) = self.entry else {
            return Ok(());
        };
        self.remaining = store.blocks()?;
        self.block = entry.head;
        if let Some(after) = self.after
            && ordered(entry.first, after)? != Ordering::Greater
        {
            if entry.tail == NO_BLOCK {
                return Ok(());
            }
            let tail = load_posting(store, entry.tail, entry.reference)?;
            if let Some((first, last)) = endpoints(&tail)? {
                if ordered(entry.first, first)? != Ordering::Less {
                    return Err(Error::InvalidState);
                }
                if ordered(last, after)? != Ordering::Greater {
                    self.block = NO_BLOCK;
                    return Ok(());
                }
                // all predecessors are old only when this page straddles the cutoff.
                if entry.head == entry.tail || ordered(first, after)? != Ordering::Greater {
                    self.block = entry.tail;
                    self.page = Some(OwnedPostings::new(tail)?);
                }
                // a wholly newer tail can have newer predecessors; retain the head walk.
            }
        }
        self.current = Some(entry.first);
        self.previous = self.current;
        self.seek_open(store, self.after, true)?;
        Ok(())
    }

    fn advance<S: PageStore>(
        &mut self,
        store: &mut S,
        target: OwnerRef,
        exclusive: bool,
    ) -> Result<Option<OwnerRef>> {
        let entry = self.entry.ok_or(Error::InvalidState)?;
        loop {
            if let Some(page) = &mut self.page {
                if page.page().kind() == PageKind::DirectPostings {
                    let (_, last) = page.page().direct_endpoints()?;
                    let order = ordered(last, target)?;
                    if order == Ordering::Less || (exclusive && order == Ordering::Equal) {
                        if ordered(self.previous.ok_or(Error::InvalidState)?, last)?
                            == Ordering::Greater
                        {
                            return Err(Error::InvalidState);
                        }
                        self.previous = Some(last);
                        self.block = posting_next(page.page(), entry.tail, &mut self.remaining)?
                            .unwrap_or(NO_BLOCK);
                        self.page = None;
                        continue;
                    }
                }
                if let Some(owner) = page.next() {
                    let owner = owner?;
                    if ordered(self.previous.ok_or(Error::InvalidState)?, owner)?
                        != Ordering::Less
                    {
                        return Err(Error::InvalidState);
                    }
                    self.previous = Some(owner);
                    return Ok(Some(owner));
                }
                self.block = posting_next(page.page(), entry.tail, &mut self.remaining)?
                    .unwrap_or(NO_BLOCK);
                self.page = None;
            }
            if self.block == NO_BLOCK {
                return Ok(None);
            }
            self.page = Some(OwnedPostings::new(load_posting(
                store,
                self.block,
                entry.reference,
            )?)?);
        }
    }

    fn seek_open<S: PageStore>(
        &mut self,
        store: &mut S,
        target: Option<OwnerRef>,
        exclusive: bool,
    ) -> Result<Option<OwnerRef>> {
        while let (Some(current), Some(target)) = (self.current, target) {
            let order = ordered(current, target)?;
            if order == Ordering::Greater || (order == Ordering::Equal && !exclusive) {
                break;
            }
            self.current = self.advance(store, target, exclusive)?;
        }
        Ok(self.current)
    }

    pub(super) fn seek<S: PageStore>(
        &mut self,
        store: &mut S,
        target: Option<OwnerRef>,
        exclusive: bool,
    ) -> Result<Option<OwnerRef>> {
        if !self.opened {
            self.open(store)?;
        }
        let result = self.seek_open(store, target, exclusive)?;
        if let (Some(owner), Some(after)) = (result, self.after)
            && ordered(owner, after)? != Ordering::Greater
        {
            return Err(Error::InvalidState);
        }
        Ok(result)
    }
}

// direct pages expose endpoints; other codecs use one bounded, checked page walk.
fn endpoints(page: &Page) -> Result<Option<(OwnerRef, OwnerRef)>> {
    if page.kind() == PageKind::DirectPostings {
        return page.direct_endpoints().map(Some);
    }
    let mut owners = page.posting_refs()?;
    let Some(first) = owners.next().transpose()? else {
        return Ok(None);
    };
    let last = owners.try_fold(first, |_, owner| owner)?;
    Ok(Some((first, last)))
}

// owner coordinates and never-reused incarnations must have the same order.
fn ordered(left: OwnerRef, right: OwnerRef) -> Result<Ordering> {
    let order = (left.page, left.slot).cmp(&(right.page, right.slot));
    if order != left.incarnation.cmp(&right.incarnation) {
        return Err(Error::InvalidState);
    }
    Ok(order)
}
