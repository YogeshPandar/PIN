//! term-addressed suffixes beyond the grouped snapshot's incarnation fence.
//! exact boolean membership never substitutes for host snapshot visibility.

use super::{NODES, Program};
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::grouped::Node;
use crate::identity::RootTid;
use crate::mutable::page::{
    GroupSnapshot, NO_BLOCK, OwnedPostings, OwnerRef, Page, PageKind, Term, TermRef,
};
use crate::mutable::{PageStore, following, load, load_posting, posting_next, reader::resolve};

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
}

struct Cursor {
    term: Option<CapturedTerm>,
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
    fn new(term: Option<CapturedTerm>) -> Self {
        Self {
            term,
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
        // a suffix spanning pages has no persisted seek anchor; retain the full walk.
        self.block = term.head;
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
            slot: snapshot.after.map_or(0, |owner| owner.slot + 1),
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

pub(super) fn memory(terms: usize) -> usize {
    terms * core::mem::size_of::<Cursor>() + 128 * 1024
}

// grouped scratch has been released; metadata and posting tails remain captured.
pub(super) fn scan<S: PageStore>(
    store: &mut S,
    program: &Program<'_>,
    captured: &[Option<CapturedTerm>],
    meta: &Page,
    snapshot: GroupSnapshot,
    mut emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<u64> {
    if !meta.grouped_has_delta(snapshot)? {
        return Ok(0);
    }
    let mut cursors = Vec::new();
    cursors
        .try_reserve_exact(captured.len())
        .map_err(|_| Error::Allocation)?;
    for &term in captured {
        cursors.push(Cursor::new(term));
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
