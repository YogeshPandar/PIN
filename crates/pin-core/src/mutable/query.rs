//! Bounded streaming intersections and unions over stable owner identities.
//! Positive Boolean plans can prove membership; covers retain recheck obligations.
//! Neither path evaluates SQL visibility. The opt-in phrase path proves
//! positional membership from complete inline or fragmented payloads.
//! Contracts: docs/g4-query-execution.md and PostgreSQL 18 index-scanning.

use super::document;
use super::page::{NO_BLOCK, OwnedPostings, OwnerRef, Page, PageKind, Term, TermRef};
use super::reader::resolve;
use super::{PageStore, find_term, load, load_into, load_posting, posting_next, scan};
use crate::budget::MemoryBudget;
use crate::candidate::CandidatePlan;
use crate::error::{Error, Result};
use crate::identity::RootTid;
use crate::memory::{release, vector};
use crate::query::{Kind, Query};
use std::cmp::Ordering;

const EMPTY: usize = 0;
const UNIVERSE: usize = 1;
const UNUSED: usize = usize::MAX;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Node<'q> {
    Empty,
    Universe,
    Term { text: &'q str, cursor: usize },
    And(usize, usize),
    Or(usize, usize),
}

#[derive(Clone, Copy)]
enum Task {
    Seek {
        node: usize,
        target: Option<OwnerRef>,
        exclusive: bool,
    },
    AndLeft {
        node: usize,
        right: usize,
    },
    AndRight {
        node: usize,
        left: OwnerRef,
    },
    OrLeft {
        right: usize,
        target: Option<OwnerRef>,
        exclusive: bool,
    },
    OrRight {
        left: Option<OwnerRef>,
    },
}

struct Plan<'q> {
    nodes: Vec<Node<'q>>,
    cursors: Vec<Cursor>,
    tasks: Vec<Task>,
    root: usize,
    scratch_bytes: usize,
}

impl<'q> Plan<'q> {
    fn build(query: &'q Query, memory_bytes: usize) -> Result<Self> {
        let mut budget = MemoryBudget::new(memory_bytes);
        let mut capacity = query
            .node_count()
            .checked_add(2)
            .ok_or(Error::Limit("query nodes"))?;
        for node in &query.nodes {
            if let Kind::Phrase(terms) = &node.kind {
                capacity = terms
                    .len()
                    .checked_mul(2)
                    .and_then(|extra| capacity.checked_add(extra))
                    .ok_or(Error::Limit("query nodes"))?;
            }
        }
        let mut nodes = vector(capacity, &mut budget)?;
        let mut mapped: Vec<usize> = vector(query.node_count(), &mut budget)?;
        nodes.extend([Node::Empty, Node::Universe]);
        for node in &query.nodes {
            let index = match &node.kind {
                Kind::None => EMPTY,
                Kind::Prefix(_) | Kind::Not(_) => UNIVERSE,
                Kind::Term(text) => term(&mut nodes, text),
                Kind::Phrase(terms) => {
                    let mut result = UNIVERSE;
                    for text in terms {
                        let next = term(&mut nodes, text);
                        result = combine(&mut nodes, result, next, true);
                    }
                    if terms.is_empty() { EMPTY } else { result }
                }
                Kind::And(left, right) => combine(&mut nodes, mapped[*left], mapped[*right], true),
                Kind::Or(left, right) => combine(&mut nodes, mapped[*left], mapped[*right], false),
            };
            mapped.push(index);
        }
        let root = mapped[query.root];
        release(mapped, &mut budget)?;
        // mark only reachable operands; NOT and universe simplification discard children.
        let mut active = vector(nodes.len(), &mut budget)?;
        active.resize(nodes.len(), false);
        active[root] = true;
        let mut count = 0usize;
        for index in (0..nodes.len()).rev() {
            if !active[index] {
                continue;
            }
            match nodes[index] {
                Node::And(left, right) | Node::Or(left, right) => {
                    active[left] = true;
                    active[right] = true;
                }
                Node::Term { .. } => count += 1,
                Node::Empty | Node::Universe => {}
            }
        }
        // private encoded pages are included in the actual cursor vector capacity.
        let mut cursors = vector(count, &mut budget)?;
        for (index, node) in nodes.iter_mut().enumerate() {
            if let Node::Term { cursor, .. } = node
                && active[index]
            {
                *cursor = cursors.len();
                cursors.push(Cursor::empty());
            }
        }
        release(active, &mut budget)?;
        // direct roots never enter the continuation interpreter.
        let direct = match nodes[root] {
            Node::And(left, right) | Node::Or(left, right) => {
                matches!(
                    (nodes[left], nodes[right]),
                    (Node::Term { .. }, Node::Term { .. })
                )
            }
            Node::Empty | Node::Universe | Node::Term { .. } => true,
        };
        let tasks = if direct {
            Vec::new()
        } else {
            vector(nodes.len(), &mut budget)?
        };
        Ok(Self {
            nodes,
            cursors,
            tasks,
            root,
            scratch_bytes: budget.remaining(),
        })
    }

    fn open<S: PageStore>(&mut self, store: &mut S) -> Result<()> {
        let meta = load(store, 0, PageKind::Meta)?;
        for index in 0..self.nodes.len() {
            let Node::Term { text, cursor } = self.nodes[index] else {
                continue;
            };
            if cursor == UNUSED {
                continue;
            }
            // reuse immutable term metadata; duplicate cursors still advance independently.
            if self.nodes[..index].iter().any(|node| {
                matches!(
                    *node,
                    Node::Term {
                        text: previous,
                        cursor
                    } if cursor != UNUSED && previous == text
                )
            }) {
                continue;
            }
            store.interrupt()?;
            let Some((dictionary, reference)) = find_term(store, &meta, text)? else {
                continue;
            };
            let entry = dictionary.term(reference)?;
            let first = entry.first;
            let head = entry.head;
            let tail = entry.tail;
            let remaining = store.blocks()?;
            for node in &self.nodes[index..] {
                let Node::Term {
                    text: duplicate,
                    cursor,
                } = *node
                else {
                    continue;
                };
                if cursor != UNUSED && duplicate == text {
                    self.cursors[cursor] = Cursor {
                        current: Some(first),
                        chain: Some(Chain {
                            reference,
                            block: head,
                            tail,
                            remaining,
                            previous: first,
                            page: None,
                        }),
                    };
                }
            }
        }
        Ok(())
    }

    fn seek<S: PageStore>(
        &mut self,
        store: &mut S,
        target: Option<OwnerRef>,
    ) -> Result<Option<OwnerRef>> {
        // bypass the continuation stack for common one- and two-term roots.
        match self.nodes[self.root] {
            Node::Term { cursor, .. } => {
                return self.cursors[cursor].seek(store, target, true);
            }
            Node::And(left, right) => {
                if let (
                    Node::Term {
                        cursor: left_cursor,
                        ..
                    },
                    Node::Term {
                        cursor: right_cursor,
                        ..
                    },
                ) = (self.nodes[left], self.nodes[right])
                {
                    return seek_pair_and(
                        &mut self.cursors,
                        store,
                        left_cursor,
                        right_cursor,
                        target,
                    );
                }
            }
            Node::Or(left, right) => {
                if let (
                    Node::Term {
                        cursor: left_cursor,
                        ..
                    },
                    Node::Term {
                        cursor: right_cursor,
                        ..
                    },
                ) = (self.nodes[left], self.nodes[right])
                {
                    return seek_pair_or(
                        &mut self.cursors,
                        store,
                        left_cursor,
                        right_cursor,
                        target,
                    );
                }
            }
            Node::Empty | Node::Universe => return Err(Error::InvalidState),
        }
        self.tasks.clear();
        self.tasks.push(Task::Seek {
            node: self.root,
            target,
            exclusive: true,
        });
        let mut value = None;
        // each pending continuation belongs to a distinct ancestor, so nodes bound scratch.
        let mut work = 0u8;
        while let Some(task) = self.tasks.pop() {
            work = work.wrapping_add(1);
            if work == 0 {
                store.interrupt()?;
            }
            match task {
                Task::Seek {
                    node,
                    target,
                    exclusive,
                } => match self.nodes[node] {
                    Node::Term { cursor, .. } => {
                        value = self.cursors[cursor].seek(store, target, exclusive)?;
                    }
                    Node::And(left, right) => {
                        self.tasks.push(Task::AndLeft { node, right });
                        self.tasks.push(Task::Seek {
                            node: left,
                            target,
                            exclusive,
                        });
                    }
                    Node::Or(left, right) => {
                        self.tasks.push(Task::OrLeft {
                            right,
                            target,
                            exclusive,
                        });
                        self.tasks.push(Task::Seek {
                            node: left,
                            target,
                            exclusive,
                        });
                    }
                    Node::Empty => value = None,
                    Node::Universe => return Err(Error::InvalidState),
                },
                Task::AndLeft { node, right } => {
                    if let Some(left) = value {
                        self.tasks.push(Task::AndRight { node, left });
                        self.tasks.push(Task::Seek {
                            node: right,
                            target: Some(left),
                            exclusive: false,
                        });
                    }
                }
                Task::AndRight { node, left } => {
                    if let Some(right) = value {
                        match compare(left, right)? {
                            Ordering::Equal => value = Some(left),
                            Ordering::Less => self.tasks.push(Task::Seek {
                                node,
                                target: Some(right),
                                exclusive: false,
                            }),
                            Ordering::Greater => return Err(Error::InvalidState),
                        }
                    }
                }
                Task::OrLeft {
                    right,
                    target,
                    exclusive,
                } => {
                    self.tasks.push(Task::OrRight { left: value });
                    self.tasks.push(Task::Seek {
                        node: right,
                        target,
                        exclusive,
                    });
                }
                Task::OrRight { left } => {
                    value = match (left, value) {
                        (Some(left), Some(right)) => {
                            Some(if compare(left, right)? == Ordering::Greater {
                                right
                            } else {
                                left
                            })
                        }
                        (left, right) => left.or(right),
                    };
                }
            }
        }
        Ok(value)
    }
}

fn term<'q>(nodes: &mut Vec<Node<'q>>, text: &'q str) -> usize {
    let index = nodes.len();
    nodes.push(Node::Term {
        text,
        cursor: UNUSED,
    });
    index
}

fn combine<'q>(nodes: &mut Vec<Node<'q>>, left: usize, right: usize, and: bool) -> usize {
    if nodes[left] == nodes[right] {
        return left;
    }
    let (identity, absorbing) = if and {
        (UNIVERSE, EMPTY)
    } else {
        (EMPTY, UNIVERSE)
    };
    if left == absorbing || right == absorbing {
        return absorbing;
    }
    if left == identity {
        return right;
    }
    if right == identity {
        return left;
    }
    let index = nodes.len();
    nodes.push(if and {
        Node::And(left, right)
    } else {
        Node::Or(left, right)
    });
    index
}

struct Cursor {
    current: Option<OwnerRef>,
    chain: Option<Chain>,
}

struct Chain {
    reference: TermRef,
    block: u32,
    tail: u32,
    remaining: u32,
    previous: OwnerRef,
    page: Option<OwnedPostings>,
}

impl Cursor {
    fn empty() -> Self {
        Self {
            current: None,
            chain: None,
        }
    }

    fn seek<S: PageStore>(
        &mut self,
        store: &mut S,
        target: Option<OwnerRef>,
        exclusive: bool,
    ) -> Result<Option<OwnerRef>> {
        while let (Some(current), Some(target)) = (self.current, target) {
            let order = compare(current, target)?;
            if order == Ordering::Greater || (order == Ordering::Equal && !exclusive) {
                break;
            }
            let chain = self.chain.as_mut().ok_or(Error::InvalidState)?;
            chain.skip_direct_before(target, exclusive)?;
            self.current = chain.advance(store)?;
        }
        Ok(self.current)
    }
}

fn cursor_pair(
    cursors: &mut [Cursor],
    left: usize,
    right: usize,
) -> Result<(&mut Cursor, &mut Cursor)> {
    if left == right || left >= cursors.len() || right >= cursors.len() {
        return Err(Error::InvalidState);
    }
    if left < right {
        let (before, after) = cursors.split_at_mut(right);
        Ok((&mut before[left], &mut after[0]))
    } else {
        let (before, after) = cursors.split_at_mut(left);
        Ok((&mut after[0], &mut before[right]))
    }
}

fn seek_pair_and<S: PageStore>(
    cursors: &mut [Cursor],
    store: &mut S,
    left: usize,
    right: usize,
    target: Option<OwnerRef>,
) -> Result<Option<OwnerRef>> {
    let (left, right) = cursor_pair(cursors, left, right)?;
    let mut left_value = left.seek(store, target, true)?;
    let mut right_value = right.seek(store, target, true)?;
    let mut work = 0u8;
    loop {
        let (Some(left_owner), Some(right_owner)) = (left_value, right_value) else {
            return Ok(None);
        };
        match compare(left_owner, right_owner)? {
            Ordering::Equal => return Ok(Some(left_owner)),
            Ordering::Less => {
                left_value = left.seek(store, Some(right_owner), false)?;
            }
            Ordering::Greater => {
                right_value = right.seek(store, Some(left_owner), false)?;
            }
        }
        work = work.wrapping_add(1);
        if work == 0 {
            store.interrupt()?;
        }
    }
}

fn seek_pair_or<S: PageStore>(
    cursors: &mut [Cursor],
    store: &mut S,
    left: usize,
    right: usize,
    target: Option<OwnerRef>,
) -> Result<Option<OwnerRef>> {
    let (left, right) = cursor_pair(cursors, left, right)?;
    let left = left.seek(store, target, true)?;
    let right = right.seek(store, target, true)?;
    match (left, right) {
        (Some(left), Some(right)) => Ok(Some(if compare(left, right)? == Ordering::Greater {
            right
        } else {
            left
        })),
        (left, right) => Ok(left.or(right)),
    }
}

impl Chain {
    fn skip_direct_before(&mut self, target: OwnerRef, exclusive: bool) -> Result<()> {
        let Some(page) = self.page.as_ref() else {
            return Ok(());
        };
        if page.page().kind() != PageKind::DirectPostings {
            return Ok(());
        }
        let (_, last) = page.page().direct_endpoints()?;
        let order = compare(last, target)?;
        if order == Ordering::Greater || (order == Ordering::Equal && !exclusive) {
            return Ok(());
        }
        if compare(self.previous, last)? == Ordering::Greater
            || self.previous.incarnation > last.incarnation
        {
            return Err(Error::InvalidState);
        }
        self.previous = last;
        self.block = posting_next(page.page(), self.tail, &mut self.remaining)?.unwrap_or(NO_BLOCK);
        self.page = None;
        Ok(())
    }

    fn advance<S: PageStore>(&mut self, store: &mut S) -> Result<Option<OwnerRef>> {
        loop {
            if let Some(page) = &mut self.page {
                if let Some(owner) = page.next() {
                    let owner = owner?;
                    if compare(self.previous, owner)? != Ordering::Less
                        || owner.incarnation.get() <= self.previous.incarnation.get()
                    {
                        return Err(Error::InvalidState);
                    }
                    self.previous = owner;
                    return Ok(Some(owner));
                }
                self.block =
                    posting_next(page.page(), self.tail, &mut self.remaining)?.unwrap_or(NO_BLOCK);
                self.page = None;
            }
            if self.block == NO_BLOCK {
                return Ok(None);
            }
            self.page = Some(OwnedPostings::new(load_posting(
                store,
                self.block,
                self.reference,
            )?)?);
        }
    }
}

// equal coordinates must identify the same never-reused owner incarnation.
fn compare(left: OwnerRef, right: OwnerRef) -> Result<Ordering> {
    let order = (left.page, left.slot).cmp(&(right.page, right.slot));
    if order == Ordering::Equal && left.incarnation != right.incarnation {
        return Err(Error::InvalidState);
    }
    Ok(order)
}

/// Streams Boolean necessary conditions with bounded private posting cursors.
///
/// AND intersects, OR unions, and phrases intersect their required terms. Prefix
/// and NOT operands conservatively cover the universe. The host must hold the
/// shared structural barrier throughout this call and recheck every heap tuple.
/// The memory budget excludes the borrowed query, fixed page scratch and host
/// bitmap. A cursor-budget failure uses the original term cover, never truncation.
/// Returned accounting is not a snapshot-visible count; the host deduplicates TIDs.
///
/// # Errors
/// Rejects encountered corruption, invalid owner order, host failures and budgets
/// too small for even the fallback. No fallback is attempted after any emission.
pub fn scan_query<S: PageStore>(
    store: &mut S,
    query: &Query,
    memory_bytes: usize,
    mut emit: impl FnMut(RootTid) -> Result<()>,
) -> Result<u64> {
    scan_query_with_recheck(store, query, memory_bytes, |root, _| emit(root))
}

/// Streams roots with the predicate recheck obligation of the executed plan.
///
/// A false flag proves only positive term/AND/OR membership for this query,
/// after published-owner and incarnation validation. It never proves visibility.
/// The host must still enforce all other scan keys and SQL qualifications.
/// Approximate operators and every budget fallback emit with recheck required.
/// PostgreSQL bitmap lossification may independently require rechecks.
///
/// # Errors
/// Has the same bounded-work and structural-barrier contract as `scan_query`.
pub fn scan_query_with_recheck<S: PageStore>(
    store: &mut S,
    query: &Query,
    memory_bytes: usize,
    emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<u64> {
    scan_query_with_options(store, query, memory_bytes, false, emit)
}

/// optionally proves a single phrase from a complete indexed positional payload.
/// payloads larger than the scan budget retain the heap predicate recheck.
pub fn scan_query_with_options<S: PageStore>(
    store: &mut S,
    query: &Query,
    memory_bytes: usize,
    phrase_positions: bool,
    mut emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<u64> {
    if let Kind::Term(text) = &query.nodes[query.root].kind {
        return scan_single_term(store, text, |root| emit(root, false));
    }
    let mut plan = match Plan::build(query, memory_bytes) {
        Ok(plan) => plan,
        Err(Error::Budget(_)) => {
            let cover = CandidatePlan::build(query, memory_bytes)?;
            return scan(store, &cover, |root| emit(root, true));
        }
        Err(error) => return Err(error),
    };
    if plan.root == EMPTY {
        return Ok(0);
    }
    if plan.root == UNIVERSE {
        return scan(store, &CandidatePlan::Universe, |root| emit(root, true));
    }
    let recheck = query.nodes.iter().any(|node| {
        !matches!(
            node.kind,
            Kind::None | Kind::Term(_) | Kind::And(_, _) | Kind::Or(_, _)
        )
    });
    let exact_phrase = if phrase_positions && query.node_count() == 1 {
        match &query.nodes[query.root].kind {
            Kind::Phrase(terms) if !terms.is_empty() && terms.len() <= 64 => Some(terms.as_slice()),
            _ => None,
        }
    } else {
        None
    };
    plan.open(store)?;
    let mut payload = Vec::new();
    let mut fragment_cache = None;
    let mut previous = None;
    let mut cache: Option<Page> = None;
    let mut count = 0u64;
    while let Some(owner) = plan.seek(store, previous)? {
        if let Some(previous) = previous
            && compare(previous, owner)? != Ordering::Less
        {
            return Err(Error::InvalidState);
        }
        previous = Some(owner);
        if let Some(terms) = exact_phrase {
            if let Some((root, needs_recheck)) = resolve_phrase(
                store,
                &mut cache,
                owner,
                terms,
                plan.scratch_bytes,
                &mut payload,
                &mut fragment_cache,
            )? {
                emit(root, needs_recheck)?;
                count = count
                    .checked_add(1)
                    .ok_or(Error::Limit("candidate count"))?;
            }
            continue;
        }
        let mut direct = None;
        for cursor in &plan.cursors {
            if cursor.current == Some(owner)
                && let Some(page) = cursor.chain.as_ref().and_then(|chain| chain.page.as_ref())
            {
                direct = page.current_direct(store.layout())?;
                if direct.is_some() {
                    break;
                }
            }
        }
        let root = match direct {
            Some(root) => root,
            None => resolve(store, &mut cache, owner)?,
        };
        if let Some(root) = root {
            emit(root, recheck)?;
            count = count
                .checked_add(1)
                .ok_or(Error::Limit("candidate count"))?;
        }
    }
    Ok(count)
}

fn resolve_phrase<S: PageStore>(
    store: &mut S,
    cache: &mut Option<Page>,
    reference: OwnerRef,
    terms: &[String],
    memory_bytes: usize,
    bytes: &mut Vec<u8>,
    fragment_cache: &mut Option<Page>,
) -> Result<Option<(RootTid, bool)>> {
    let reload = match cache.as_ref() {
        Some(page) if page.block() == reference.page => reference.slot >= page.owner_count()?,
        _ => true,
    };
    if reload {
        if let Some(page) = cache.as_mut() {
            load_into(store, reference.page, PageKind::Owners, page)?;
        } else {
            *cache = Some(load(store, reference.page, PageKind::Owners)?);
        }
    }
    let page = cache.as_ref().ok_or(Error::InvalidState)?;
    let owner = page.owner(reference.slot, store.layout())?;
    if owner.reference != reference {
        return Err(Error::InvalidState);
    }
    if !owner.live || owner.publication != crate::codec::records::Publication::Published {
        return Ok(None);
    }
    if !owner.inline.is_empty() {
        return Ok(super::phrase_prefix::matches(
            owner.inline,
            owner.inline.len(),
            owner.tokens,
            owner.terms,
            terms,
        )?
        .ok_or(Error::InvalidState)?
        .then_some((owner.root, false)));
    }
    let total = usize::try_from(owner.data_bytes).map_err(|_| Error::InvalidState)?;
    let Some(memory_bytes) = memory_bytes.checked_sub(std::mem::size_of::<Page>()) else {
        return Ok(Some((owner.root, true)));
    };
    if total.max(bytes.capacity()) > memory_bytes || total > document::MAX_DOCUMENT_BYTES {
        return Ok(Some((owner.root, true)));
    }
    if let Some(page) = fragment_cache.as_mut() {
        load_into(store, owner.data_head, PageKind::Fragment, page)?;
    } else {
        *fragment_cache = Some(load(store, owner.data_head, PageKind::Fragment)?);
    }
    let first = fragment_cache.as_mut().ok_or(Error::InvalidState)?;
    let (identity, start, payload) = first.fragment_data()?;
    if identity != reference || start != 0 || payload.len() > total {
        return Err(Error::InvalidState);
    }
    if let Some(matched) =
        super::phrase_prefix::matches(payload, total, owner.tokens, owner.terms, terms)?
    {
        return Ok(matched.then_some((owner.root, false)));
    }
    bytes.clear();
    if bytes.try_reserve_exact(total).is_err() || bytes.capacity() > memory_bytes {
        *bytes = Vec::new();
        return Ok(Some((owner.root, true)));
    }
    bytes.extend_from_slice(payload);
    let mut block = first.next()?;
    let mut offset = payload.len();
    let mut remaining = store.blocks()?;
    while block != NO_BLOCK {
        remaining = remaining.checked_sub(1).ok_or(Error::InvalidState)?;
        if offset == total {
            return Err(Error::InvalidState);
        }
        load_into(store, block, PageKind::Fragment, first)?;
        let (reference, current, payload) = first.fragment_data()?;
        let current = usize::try_from(current).map_err(|_| Error::InvalidState)?;
        let end = current
            .checked_add(payload.len())
            .ok_or(Error::InvalidState)?;
        if reference != owner.reference || current != offset || end > total {
            return Err(Error::InvalidState);
        }
        bytes.extend_from_slice(payload);
        offset = end;
        block = first.next()?;
    }
    if offset != total {
        return Err(Error::InvalidState);
    }
    Ok(
        super::phrase_prefix::matches(bytes, total, owner.tokens, owner.terms, terms)?
            .ok_or(Error::InvalidState)?
            .then_some((owner.root, false)),
    )
}

// direct pages already carry checked roots; mutable pages still resolve owners.
// the caller supplies the query's complete single-term predicate proof.
fn scan_single_term<S: PageStore>(
    store: &mut S,
    text: &str,
    mut emit: impl FnMut(RootTid) -> Result<()>,
) -> Result<u64> {
    let meta = load(store, 0, PageKind::Meta)?;
    let Some((dictionary, reference)) = find_term(store, &meta, text)? else {
        return Ok(0);
    };
    scan_term_entry(store, dictionary.term(reference)?, None, &mut emit)
}

// reuse captured term metadata and an optional validated head page before emission.
// the caller retains the structural barrier and proves a complete term predicate.
pub(super) fn scan_term_entry<S: PageStore>(
    store: &mut S,
    term: Term<'_>,
    mut first_page: Option<Page>,
    mut emit: impl FnMut(RootTid) -> Result<()>,
) -> Result<u64> {
    let reference = term.reference;
    if let Some(page) = &first_page
        && (page.block() != term.head || page.posting_term()? != reference)
    {
        return Err(Error::InvalidState);
    }
    let mut count = 0u64;
    let mut owner_page = None;
    if let Some(root) = resolve(store, &mut owner_page, term.first)? {
        emit(root)?;
        count = 1;
    }
    if term.head == NO_BLOCK {
        return Ok(count);
    }
    let mut block = term.head;
    let mut remaining = store.blocks()?;
    let mut previous = term.first;
    loop {
        let page = match first_page.take() {
            Some(page) => page,
            None => load_posting(store, block, reference)?,
        };
        if page.kind() == PageKind::DirectPostings {
            let (first, last) = page.direct_endpoints()?;
            if compare(previous, first)? != Ordering::Less
                || first.incarnation <= previous.incarnation
            {
                return Err(Error::InvalidState);
            }
            previous = last;
            let slots = page.posting_count()?;
            for slot in 0..slots {
                if let Some(root) = page.direct_root(slot, store.layout())? {
                    emit(root)?;
                    count = count
                        .checked_add(1)
                        .ok_or(Error::Limit("candidate count"))?;
                }
            }
        } else {
            for owner in page.posting_refs()? {
                let owner = owner?;
                if compare(previous, owner)? != Ordering::Less
                    || owner.incarnation <= previous.incarnation
                {
                    return Err(Error::InvalidState);
                }
                previous = owner;
                if let Some(root) = resolve(store, &mut owner_page, owner)? {
                    emit(root)?;
                    count = count
                        .checked_add(1)
                        .ok_or(Error::Limit("candidate count"))?;
                }
            }
        }
        match posting_next(&page, term.tail, &mut remaining)? {
            Some(next) => block = next,
            None => break,
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryLimits;

    #[test]
    fn direct_roots_do_not_allocate_continuations() {
        for source in [
            "", "a", "a AND b", "a OR b", "a AND a", "\"a b\"", "NOT a", "a*",
        ] {
            let query = Query::parse(source, QueryLimits::default()).unwrap();
            let plan = Plan::build(&query, 1 << 20).unwrap();
            assert_eq!(plan.tasks.capacity(), 0, "{source}");
        }
        let query = Query::parse("(a OR b) AND c", QueryLimits::default()).unwrap();
        let plan = Plan::build(&query, 1 << 20).unwrap();
        assert!(plan.tasks.capacity() >= plan.nodes.len());
    }
}
