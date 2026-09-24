//! exact Boolean membership over owners newer than a grouped snapshot.
//! no index bit certifies heap visibility; all output uses the existing bitmap sink.

use super::scan::{NODES, Program};
use crate::budget::MemoryBudget;
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::grouped::Node;
use crate::identity::RootTid;
use crate::memory::vector;
use crate::mutable::page::{GroupSnapshot, NO_BLOCK, OwnerRef, Page, PageKind};
use crate::mutable::frontier::{SuffixCursor, SuffixTerm};
use crate::mutable::{PageStore, following, load};
use std::cmp::Ordering;

#[derive(Clone, Copy)]
enum Bound {
    Empty,
    Owner(OwnerRef),
    Any,
}

fn compare(left: OwnerRef, right: OwnerRef) -> Result<Ordering> {
    let order = (left.page, left.slot).cmp(&(right.page, right.slot));
    if order != left.incarnation.cmp(&right.incarnation) {
        return Err(Error::InvalidState);
    }
    Ok(order)
}

fn combine(left: Bound, right: Bound, and: bool) -> Result<Bound> {
    match (left, right) {
        (Bound::Empty, other) | (other, Bound::Empty) => {
            Ok(if and { Bound::Empty } else { other })
        }
        (Bound::Any, other) | (other, Bound::Any) => Ok(if and { other } else { Bound::Any }),
        (Bound::Owner(left), Bound::Owner(right)) => {
            let greater = compare(left, right)? == Ordering::Greater;
            Ok(Bound::Owner(if greater == and { left } else { right }))
        }
    }
}

struct Owners {
    page: Page,
    slot: u16,
    tail: u32,
    current: Option<(OwnerRef, RootTid)>,
    done: bool,
    snapshot: GroupSnapshot,
}

impl Owners {
    fn open<S: PageStore>(store: &mut S, meta: &Page, snapshot: GroupSnapshot) -> Result<Option<Self>> {
        let (head, tail) = meta.owner_chain()?;
        if head == NO_BLOCK {
            return Ok(None);
        }
        let page = load(store, snapshot.after.map_or(head, |owner| owner.page), PageKind::Owners)?;
        let slot = match snapshot.after {
            Some(after) => {
                if page.owner(after.slot, store.layout())?.reference != after {
                    return Err(Error::InvalidState);
                }
                after.slot.checked_add(1).ok_or(Error::InvalidState)?
            }
            None => 0,
        };
        if page.block() == tail && slot == page.owner_count()? {
            return Ok(None);
        }
        Ok(Some(Self {
            page,
            slot,
            tail,
            current: None,
            done: false,
            snapshot,
        }))
    }

    fn advance<S: PageStore>(&mut self, store: &mut S) -> Result<()> {
        self.current = None;
        while !self.done {
            while self.slot < self.page.owner_count()? {
                let owner = self.page.owner(self.slot, store.layout())?;
                self.slot += 1;
                if owner.reference.incarnation.get() <= self.snapshot.id.get() {
                    return Err(Error::InvalidState);
                }
                if owner.publication == Publication::Published && owner.live {
                    self.current = Some((owner.reference, owner.root));
                    return Ok(());
                }
            }
            match following(&self.page, self.tail)? {
                Some(block) => {
                    self.page = load(store, block, PageKind::Owners)?;
                    self.slot = 0;
                }
                None => self.done = true,
            }
        }
        Ok(())
    }

    fn seek<S: PageStore>(
        &mut self,
        store: &mut S,
        target: Option<OwnerRef>,
        exclusive: bool,
    ) -> Result<Option<OwnerRef>> {
        loop {
            if let Some((current, _)) = self.current {
                let order = target.map(|target| compare(current, target)).transpose()?;
                if order.is_none_or(|order| {
                    order == Ordering::Greater || (order == Ordering::Equal && !exclusive)
                }) {
                    return Ok(Some(current));
                }
            } else if self.done {
                return Ok(None);
            }
            self.advance(store)?;
        }
    }
}

fn next<S: PageStore>(
    store: &mut S,
    program: &Program<'_>,
    cursors: &mut [SuffixCursor],
    owners: &mut Owners,
    mut target: Option<OwnerRef>,
) -> Result<Option<OwnerRef>> {
    let mut exclusive = true;
    loop {
        store.interrupt()?;
        let mut terms = [Bound::Empty; NODES];
        for (index, cursor) in cursors.iter_mut().enumerate() {
            if program.seek_terms & (1 << index) != 0 {
                terms[index] = cursor
                    .seek(store, target, exclusive)?
                    .map_or(Bound::Empty, Bound::Owner);
            }
        }
        let mut bounds = [Bound::Empty; NODES];
        for (index, node) in program.nodes[..program.len].iter().enumerate() {
            bounds[index] = match *node {
                Node::Term(term) => terms[term],
                Node::And(left, right) => combine(bounds[left], bounds[right], true)?,
                Node::Or(left, right) => combine(bounds[left], bounds[right], false)?,
                // complement cannot use its child's absence as an ordered lower bound.
                Node::Not(_) => Bound::Any,
                _ => return Err(Error::InvalidState),
            };
        }
        let candidate = match bounds[program.len - 1] {
            Bound::Empty => return Ok(None),
            Bound::Owner(owner) => Some(owner),
            Bound::Any => owners.seek(store, target, exclusive)?,
        };
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        if let Some(previous) = target {
            let order = compare(candidate, previous)?;
            if order == Ordering::Less || (exclusive && order == Ordering::Equal) {
                return Err(Error::InvalidState);
            }
            if !exclusive && order == Ordering::Equal {
                return Ok(Some(candidate));
            }
        }
        target = Some(candidate);
        exclusive = false;
    }
}

fn matches<S: PageStore>(
    store: &mut S,
    program: &Program<'_>,
    cursors: &mut [SuffixCursor],
    owner: OwnerRef,
) -> Result<bool> {
    let mut terms = [false; NODES];
    for (index, cursor) in cursors.iter_mut().enumerate() {
        terms[index] = cursor.seek(store, Some(owner), false)? == Some(owner);
    }
    let mut values = [false; NODES];
    for (index, node) in program.nodes[..program.len].iter().enumerate() {
        values[index] = match *node {
            Node::Term(term) => terms[term],
            Node::And(left, right) => values[left] && values[right],
            Node::Or(left, right) => values[left] || values[right],
            Node::Not(child) => !values[child],
            _ => return Err(Error::InvalidState),
        };
    }
    Ok(values[program.len - 1])
}

// false means budget refusal before any suffix emission; the owner cover remains valid.
// the caller's structural barrier prevents retirement and chain rewrites throughout.
pub(super) fn scan<S: PageStore>(
    store: &mut S,
    meta: &Page,
    snapshot: GroupSnapshot,
    program: &Program<'_>,
    entries: &[Option<SuffixTerm>],
    memory_bytes: usize,
    mut emit: impl FnMut(RootTid) -> Result<()>,
) -> Result<bool> {
    let Some(mut owners) = Owners::open(store, meta, snapshot)? else {
        return Ok(true);
    };
    let mut budget = MemoryBudget::new(memory_bytes.saturating_sub(128 * 1024));
    let mut cursors = match vector(entries.len(), &mut budget) {
        Ok(cursors) => cursors,
        Err(Error::Budget(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    for &entry in entries {
        cursors.push(SuffixCursor::new(entry, snapshot.after));
    }
    let mut previous = snapshot.after;
    let mut cache = None;
    while let Some(owner) = next(store, program, &mut cursors, &mut owners, previous)? {
        if owner.incarnation.get() <= snapshot.id.get() {
            return Err(Error::InvalidState);
        }
        previous = Some(owner);
        if !matches(store, program, &mut cursors, owner)? {
            continue;
        }
        let root = match owners.current {
            Some((current, root)) if current == owner => Some(root),
            _ => crate::mutable::reader::resolve(store, &mut cache, owner)?,
        };
        if let Some(root) = root {
            emit(root)?;
        }
    }
    Ok(true)
}
