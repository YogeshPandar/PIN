//! term-addressed suffixes beyond the grouped snapshot's incarnation fence.
//! exact boolean membership never substitutes for host snapshot visibility.

use super::{NODES, Program};
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::grouped::Node;
use crate::identity::RootTid;
use crate::mutable::document;
use crate::mutable::grouped::anchors;
use crate::mutable::page::{
    GroupSnapshot, NO_BLOCK, OwnedPostings, Owner, OwnerRef, Page, PageKind, Term, TermRef,
};
use crate::mutable::{
    PageStore, Stage, following, load, load_posting, posting_next, reader::resolve,
};

const OWNER_FRONTIER_MIN_SPAN: u64 = 512;
const OWNER_FRONTIER_FIXED: usize = 128 * 1024;

#[derive(Clone, Copy)]
pub(super) struct CapturedTerm {
    reference: TermRef,
    first: OwnerRef,
    head: u32,
    tail: u32,
}

impl CapturedTerm {
    pub(super) fn new(term: Term<'_>) -> Self {
        Self {
            reference: term.reference,
            first: term.first,
            head: term.head,
            tail: term.tail,
        }
    }

    fn changed<S: PageStore>(self, store: &mut S, target: u64) -> Result<bool> {
        if self.first.incarnation.get() >= target {
            return Ok(true);
        }
        if self.head == NO_BLOCK {
            return Ok(false);
        }
        let page = load_posting(store, self.tail, self.reference)?;
        let mut previous = self.first;
        let mut found = false;
        for owner in page.posting_refs()? {
            let owner = owner?;
            ordered(previous, owner)?;
            previous = owner;
            found = true;
        }
        if !found {
            return Err(Error::InvalidState);
        }
        Ok(previous.incarnation.get() >= target)
    }
}

struct Cursor {
    term: Option<CapturedTerm>,
    snapshot: GroupSnapshot,
    opened: bool,
    current: Option<OwnerRef>,
    block: u32,
    remaining: u32,
    previous: Option<OwnerRef>,
    page: Option<OwnedPostings>,
}

fn ordered(previous: OwnerRef, next: OwnerRef) -> Result<()> {
    if (previous.page, previous.slot) >= (next.page, next.slot)
        || previous.incarnation >= next.incarnation
    {
        return Err(Error::InvalidState);
    }
    Ok(())
}

impl Cursor {
    fn new(term: Option<CapturedTerm>, snapshot: GroupSnapshot) -> Self {
        Self {
            term,
            snapshot,
            opened: false,
            current: None,
            block: NO_BLOCK,
            remaining: 0,
            previous: None,
            page: None,
        }
    }

    fn open<S: PageStore>(&mut self, store: &mut S, target: u64) -> Result<()> {
        self.opened = true;
        let Some(term) = self.term else {
            return Ok(());
        };
        self.remaining = store.blocks()?;
        self.previous = Some(term.first);
        if term.first.incarnation.get() >= target {
            self.current = Some(term.first);
            self.block = term.head;
            return Ok(());
        }
        if term.head == NO_BLOCK {
            return Ok(());
        }
        let tail = load_posting(store, term.tail, term.reference)?;
        let mut records = tail.posting_refs()?;
        let first = records.next().transpose()?;
        if let Some(first) = first {
            ordered(term.first, first)?;
            let mut last = first;
            for owner in records {
                let owner = owner?;
                ordered(last, owner)?;
                last = owner;
            }
            // a captured tail fences an unchanged term without visiting owner pages.
            if last.incarnation.get() < target {
                return Ok(());
            }
            // all preceding pages end before this tail's first owner.
            if first.incarnation.get() <= target || term.head == term.tail {
                self.block = term.tail;
                self.page = Some(OwnedPostings::new(tail)?);
                return self.advance(store);
            }
        }
        if store.frontier_anchors()
            && self.snapshot.frontier_valid
            && term.first.incarnation.get() < self.snapshot.id.get()
        {
            return self.open_anchor(store, term);
        }
        // legacy and invalidated snapshots retain the complete canonical walk.
        self.block = term.head;
        self.advance(store)
    }

    fn open_anchor<S: PageStore>(&mut self, store: &mut S, term: CapturedTerm) -> Result<()> {
        let anchor = anchors::lookup(store, self.snapshot, term.reference)?;
        if anchor.head == NO_BLOCK {
            if anchor.last != term.first.incarnation.get() {
                return Err(Error::InvalidState);
            }
            self.block = term.head;
            store.event(Stage::FrontierSeek)?;
            return self.advance(store);
        }
        if anchor.head != term.head || anchor.last <= term.first.incarnation.get() {
            return Err(Error::InvalidState);
        }
        let mut page = OwnedPostings::new(load_posting(store, anchor.tail, term.reference)?)?;
        // decode at most the captured boundary page, never its historical prefix.
        loop {
            let owner = page.next().ok_or(Error::InvalidState)??;
            ordered(self.previous.ok_or(Error::InvalidState)?, owner)?;
            self.previous = Some(owner);
            if owner.incarnation.get() > anchor.last {
                return Err(Error::InvalidState);
            }
            if owner.incarnation.get() == anchor.last {
                break;
            }
        }
        self.block = anchor.tail;
        self.page = Some(page);
        store.event(Stage::FrontierSeek)?;
        self.advance(store)
    }

    fn advance<S: PageStore>(&mut self, store: &mut S) -> Result<()> {
        self.current = None;
        let term = self.term.ok_or(Error::InvalidState)?;
        loop {
            if let Some(page) = &mut self.page {
                if let Some(owner) = page.next() {
                    let owner = owner?;
                    if let Some(previous) = self.previous {
                        ordered(previous, owner)?;
                    }
                    self.previous = Some(owner);
                    self.current = Some(owner);
                    return Ok(());
                }
                self.block =
                    posting_next(page.page(), term.tail, &mut self.remaining)?.unwrap_or(NO_BLOCK);
                self.page = None;
            }
            if self.block == NO_BLOCK {
                return Ok(());
            }
            self.page = Some(OwnedPostings::new(load_posting(
                store,
                self.block,
                term.reference,
            )?)?);
        }
    }

    fn seek<S: PageStore>(&mut self, store: &mut S, target: u64) -> Result<Option<OwnerRef>> {
        if !self.opened {
            self.open(store, target)?;
        }
        let mut work = 0u8;
        while self
            .current
            .is_some_and(|owner| owner.incarnation.get() < target)
        {
            self.advance(store)?;
            work = work.wrapping_add(1);
            if work == 0 {
                store.interrupt()?;
            }
        }
        Ok(self.current)
    }
}

struct Owners {
    block: u32,
    tail: u32,
    slot: u16,
    anchor: Option<OwnerRef>,
    previous: Option<OwnerRef>,
    floor: u64,
    page: Option<Page>,
    current: Option<(OwnerRef, RootTid)>,
}

impl Owners {
    fn new(meta: &Page, snapshot: GroupSnapshot) -> Result<Self> {
        let (head, tail) = meta.owner_chain()?;
        Ok(Self {
            block: snapshot.after.map_or(head, |owner| owner.page),
            tail,
            slot: match snapshot.after {
                Some(owner) => owner.slot.checked_add(1).ok_or(Error::InvalidState)?,
                None => 0,
            },
            anchor: snapshot.after,
            previous: snapshot.after,
            floor: snapshot.id.get(),
            page: None,
            current: None,
        })
    }

    fn seek<S: PageStore>(
        &mut self,
        store: &mut S,
        target: u64,
    ) -> Result<Option<(OwnerRef, RootTid)>> {
        if self
            .current
            .is_some_and(|(owner, _)| owner.incarnation.get() >= target)
        {
            return Ok(self.current);
        }
        self.current = None;
        while self.block != NO_BLOCK {
            if self.page.is_none() {
                let page = load(store, self.block, PageKind::Owners)?;
                if let Some(anchor) = self.anchor.take()
                    && page.owner(anchor.slot, store.layout())?.reference != anchor
                {
                    return Err(Error::InvalidState);
                }
                self.page = Some(page);
            }
            let page = self.page.as_ref().ok_or(Error::InvalidState)?;
            while self.slot < page.owner_count()? {
                let owner = page.owner(self.slot, store.layout())?;
                self.slot += 1;
                if let Some(previous) = self.previous {
                    ordered(previous, owner.reference)?;
                }
                if owner.reference.incarnation.get() <= self.floor {
                    return Err(Error::InvalidState);
                }
                self.previous = Some(owner.reference);
                if owner.reference.incarnation.get() >= target
                    && owner.publication == Publication::Published
                    && owner.live
                {
                    self.current = Some((owner.reference, owner.root));
                    return Ok(self.current);
                }
            }
            self.block = following(page, self.tail)?.unwrap_or(NO_BLOCK);
            self.slot = 0;
            self.page = None;
        }
        Ok(None)
    }
}

// negation filters a candidate but supplies no positive lower bound.
fn lower_bound(program: &Program<'_>, cursors: &[Cursor], target: u64) -> Result<Option<u64>> {
    let mut bounds = [None; NODES];
    for (index, node) in program.nodes[..program.len].iter().enumerate() {
        bounds[index] = match *node {
            Node::Term(term) => cursors[term].current.map(|owner| owner.incarnation.get()),
            Node::And(left, right) => bounds[left]
                .zip(bounds[right])
                .map(|(left, right)| left.max(right)),
            Node::Or(left, right) => match (bounds[left], bounds[right]) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (left, right) => left.or(right),
            },
            Node::Not(_) => Some(target),
            _ => return Err(Error::InvalidState),
        };
    }
    Ok(bounds[program.len - 1])
}

fn matches(program: &Program<'_>, membership: u64) -> Result<bool> {
    let mut values = [false; NODES];
    for (index, node) in program.nodes[..program.len].iter().enumerate() {
        values[index] = match *node {
            Node::Term(term) => membership & (1 << term) != 0,
            Node::And(left, right) => values[left] && values[right],
            Node::Or(left, right) => values[left] || values[right],
            Node::Not(child) => !values[child],
            _ => return Err(Error::InvalidState),
        };
    }
    Ok(values[program.len - 1])
}

fn owner_frontier_span<S: PageStore>(
    store: &mut S,
    program: &Program<'_>,
    captured: &[Option<CapturedTerm>],
    meta: &Page,
    snapshot: GroupSnapshot,
) -> Result<Option<u64>> {
    if !store.owner_frontier() {
        return Ok(None);
    }
    let span = meta.grouped_delta_span(snapshot)?;
    if span < OWNER_FRONTIER_MIN_SPAN {
        return Ok(None);
    }
    if program.universe {
        return Ok(Some(span));
    }
    let target = snapshot
        .id
        .get()
        .checked_add(1)
        .ok_or(Error::InvalidState)?;
    let mut changed = 0u8;
    for term in captured.iter().flatten() {
        changed += u8::from(term.changed(store, target)?);
        if changed == 2 {
            return Ok(Some(span));
        }
    }
    Ok(None)
}

fn fragment_membership<S: PageStore>(
    store: &mut S,
    owner: Owner<'_>,
    membership: &document::TermMembership<'_, '_>,
    scratch: &mut Vec<u8>,
    scratch_limit: usize,
    max_blocks: u32,
) -> Result<Option<u64>> {
    let total = usize::try_from(owner.data_bytes).map_err(|_| Error::InvalidState)?;
    if total > scratch_limit {
        return Ok(None);
    }
    scratch.clear();
    if scratch.capacity() < total {
        if scratch.try_reserve_exact(total).is_err() {
            scratch.clear();
            return Ok(None);
        }
        if scratch.capacity() > scratch_limit {
            scratch.clear();
            return Ok(None);
        }
    }
    scratch.resize(total, 0);
    let mut block = owner.data_head;
    let mut offset = 0usize;
    let mut remaining = max_blocks;
    while block != NO_BLOCK {
        remaining = remaining.checked_sub(1).ok_or(Error::InvalidState)?;
        if offset == total {
            return Err(Error::InvalidState);
        }
        let page = load(store, block, PageKind::Fragment)?;
        let (reference, current, bytes) = page.fragment_data()?;
        let current = usize::try_from(current).map_err(|_| Error::InvalidState)?;
        let end = current
            .checked_add(bytes.len())
            .ok_or(Error::InvalidState)?;
        if reference != owner.reference || current != offset || end > total {
            return Err(Error::InvalidState);
        }
        scratch[current..end].copy_from_slice(bytes);
        offset = end;
        block = page.next()?;
    }
    if offset != total {
        return Err(Error::InvalidState);
    }
    membership
        .read(scratch, owner.tokens, owner.terms)
        .map(Some)
}

fn scan_owner_frontier<S: PageStore>(
    store: &mut S,
    program: &Program<'_>,
    meta: &Page,
    snapshot: GroupSnapshot,
    span: u64,
    memory_bytes: usize,
    mut emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<Option<u64>> {
    let ceiling = snapshot
        .id
        .get()
        .checked_add(span)
        .ok_or(Error::InvalidState)?;
    let Some(available) = memory_bytes.checked_sub(OWNER_FRONTIER_FIXED) else {
        return Ok(None);
    };
    let roots = usize::try_from(span).map_err(|_| Error::Limit("owner frontier span"))?;
    let requested = roots
        .checked_mul(core::mem::size_of::<RootTid>())
        .ok_or(Error::Limit("owner frontier roots"))?;
    if requested > available {
        return Ok(None);
    }
    let mut output = Vec::new();
    if output.try_reserve_exact(roots).is_err() {
        return Ok(None);
    }
    let retained = output
        .capacity()
        .checked_mul(core::mem::size_of::<RootTid>())
        .ok_or(Error::Limit("owner frontier roots"))?;
    if retained > available {
        return Ok(None);
    }
    let scratch_limit = available - retained;
    let mut scratch = Vec::new();
    let membership_plan = document::TermMembership::new(&program.names[..program.terms])?;
    let max_blocks = store.blocks()?;
    store.event(Stage::OwnerFrontierScan)?;

    let (head, tail) = meta.owner_chain()?;
    let mut block = snapshot.after.map_or(head, |owner| owner.page);
    let mut slot = match snapshot.after {
        Some(owner) => owner.slot.checked_add(1).ok_or(Error::InvalidState)?,
        None => 0,
    };
    let mut anchor = snapshot.after;
    let mut previous = snapshot.after;
    let mut work = 0u8;
    'owners: while block != NO_BLOCK {
        let page = load(store, block, PageKind::Owners)?;
        if let Some(expected) = anchor.take()
            && page.owner(expected.slot, store.layout())?.reference != expected
        {
            return Err(Error::InvalidState);
        }
        while slot < page.owner_count()? {
            let owner = page.owner(slot, store.layout())?;
            slot += 1;
            if let Some(previous) = previous {
                ordered(previous, owner.reference)?;
            }
            let incarnation = owner.reference.incarnation.get();
            if incarnation <= snapshot.id.get() {
                return Err(Error::InvalidState);
            }
            // stop at the metapage allocation fence captured before this scan.
            if incarnation > ceiling {
                break 'owners;
            }
            previous = Some(owner.reference);
            if owner.publication == Publication::Published && owner.live {
                let membership = if owner.inline.is_empty() {
                    let Some(membership) = fragment_membership(
                        store,
                        owner,
                        &membership_plan,
                        &mut scratch,
                        scratch_limit,
                        max_blocks,
                    )?
                    else {
                        return Ok(None);
                    };
                    membership
                } else {
                    membership_plan.read(owner.inline, owner.tokens, owner.terms)?
                };
                if matches(program, membership)? {
                    if output.len() == roots {
                        return Err(Error::InvalidState);
                    }
                    output.push(owner.root);
                }
            }
            work = work.wrapping_add(1);
            if work == 0 {
                store.interrupt()?;
            }
        }
        block = following(&page, tail)?.unwrap_or(NO_BLOCK);
        slot = 0;
    }

    let count = u64::try_from(output.len()).map_err(|_| Error::Limit("owner frontier count"))?;
    for root in output {
        emit(root, false)?;
    }
    Ok(Some(count))
}

pub(super) fn memory(terms: usize) -> usize {
    terms * core::mem::size_of::<Cursor>() + OWNER_FRONTIER_FIXED
}

// grouped scratch has been released; metadata and posting tails remain captured.
pub(super) fn scan<S: PageStore>(
    store: &mut S,
    program: &Program<'_>,
    captured: &[Option<CapturedTerm>],
    meta: &Page,
    snapshot: GroupSnapshot,
    memory_bytes: usize,
    mut emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<u64> {
    if !meta.grouped_has_delta(snapshot)? {
        return Ok(0);
    }
    if let Some(span) = owner_frontier_span(store, program, captured, meta, snapshot)?
        && let Some(count) = scan_owner_frontier(
            store,
            program,
            meta,
            snapshot,
            span,
            memory_bytes,
            &mut emit,
        )?
    {
        return Ok(count);
    }
    let mut cursors = Vec::new();
    cursors
        .try_reserve_exact(captured.len())
        .map_err(|_| Error::Allocation)?;
    for &term in captured {
        cursors.push(Cursor::new(term, snapshot));
    }
    let mut owners = Owners::new(meta, snapshot)?;
    let mut cache = None;
    let mut count = 0u64;
    let mut target = snapshot
        .id
        .get()
        .checked_add(1)
        .ok_or(Error::InvalidState)?;
    loop {
        store.interrupt()?;
        for (index, cursor) in cursors.iter_mut().enumerate() {
            if program.seek_terms & (1 << index) != 0 {
                cursor.seek(store, target)?;
            }
        }
        let Some(mut next) = lower_bound(program, &cursors, target)? else {
            break;
        };
        let mut candidate = None;
        let mut root = None;
        if program.universe {
            let Some((owner, tid)) = owners.seek(store, next)? else {
                break;
            };
            next = owner.incarnation.get();
            candidate = Some(owner);
            root = Some(tid);
        }
        if next < target {
            return Err(Error::InvalidState);
        }
        if next != target {
            target = next;
            continue;
        }
        let mut membership = 0u64;
        for (index, cursor) in cursors.iter_mut().enumerate() {
            if let Some(owner) = cursor.seek(store, target)?
                && owner.incarnation.get() == target
            {
                if candidate.is_some_and(|previous| previous != owner) {
                    return Err(Error::InvalidState);
                }
                candidate = Some(owner);
                membership |= 1 << index;
            }
        }
        if matches(program, membership)? {
            let owner = candidate.ok_or(Error::InvalidState)?;
            let root = match root {
                Some(root) => Some(root),
                None => resolve(store, &mut cache, owner)?,
            };
            if let Some(root) = root {
                emit(root, false)?;
                count = count.checked_add(1).ok_or(Error::Limit("group delta"))?;
            }
        }
        let Some(next) = target.checked_add(1) else {
            break;
        };
        target = next;
    }
    Ok(count)
}
