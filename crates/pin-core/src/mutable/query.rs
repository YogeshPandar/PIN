//! Bounded streaming intersections and unions over stable owner identities.
//! This evaluates necessary conditions, never SQL visibility or phrase positions.
//! Contracts: docs/g4-query-execution.md and PostgreSQL 18 index-scanning.

use super::page::{NO_BLOCK, OwnedPostings, OwnerRef, Page, PageKind, TermRef};
use super::reader::resolve;
use super::{PageStore, find_term, load, load_posting, posting_next, scan};
use crate::budget::MemoryBudget;
use crate::candidate::CandidatePlan;
use crate::error::{Error, Result};
use crate::identity::RootTid;
use crate::memory::vector;
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
        let tasks = vector(nodes.len(), &mut budget)?;
        Ok(Self {
            nodes,
            cursors,
            tasks,
            root,
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
            self.current = self
                .chain
                .as_mut()
                .ok_or(Error::InvalidState)?
                .advance(store)?;
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
        (Some(left), Some(right)) => Ok(Some(
            if compare(left, right)? == Ordering::Greater {
                right
            } else {
                left
            },
        )),
        (left, right) => Ok(left.or(right)),
    }
}

impl Chain {
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
    let mut plan = match Plan::build(query, memory_bytes) {
        Ok(plan) => plan,
        Err(Error::Budget(_)) => {
            let cover = CandidatePlan::build(query, memory_bytes)?;
            return scan(store, &cover, emit);
        }
        Err(error) => return Err(error),
    };
    if plan.root == EMPTY {
        return Ok(0);
    }
    if plan.root == UNIVERSE {
        return scan(store, &CandidatePlan::Universe, emit);
    }
    plan.open(store)?;
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
        if let Some(root) = resolve(store, &mut cache, owner)? {
            emit(root)?;
            count = count
                .checked_add(1)
                .ok_or(Error::Limit("candidate count"))?;
        }
    }
    Ok(count)
}
