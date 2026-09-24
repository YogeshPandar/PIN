//! page-group pruning over a complete snapshot plus an owner-ordered write delta.
//! bitmap output remains subject to PostgreSQL heap visibility and rechecks.

use super::super::page::{CatalogEntry, GroupSnapshot, NO_BLOCK, PageKind};
use super::super::{PageStore, find_term, following, load, load_posting};
use super::storage::{self, BITMAP_BYTES, Cursor, Value};
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::grouped::{Bitmap, GroupKey, Node, QueryScratch, Source, evaluate_source, needed_terms};
use crate::identity::RootTid;
use crate::query::{Kind, Query};
use pin_kernels::grouped::{OffsetMask, PageMask};

const NODES: usize = 64;
// at most one canonical posting page and 64 owner resolutions, including inline.
const SPARSE_POSTINGS: usize = 64;

struct Term {
    key: Option<u64>,
    cursor: Cursor,
    next: Option<CatalogEntry>,
    initialized: bool,
}

impl Term {
    fn at<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: GroupSnapshot,
        base: u32,
    ) -> Result<Option<CatalogEntry>> {
        let Some(key) = self.key else {
            return Ok(None);
        };
        if !self.initialized
            || self
                .next
                .is_some_and(|entry| entry.key[1] < u64::from(base))
        {
            self.next = self
                .cursor
                .seek(store, snapshot, [key, u64::from(base)])?
                .filter(|entry| entry.key[0] == key);
            self.initialized = true;
        }
        Ok(self.next.filter(|entry| entry.key[1] == u64::from(base)))
    }

    fn advance<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: GroupSnapshot,
        base: u32,
    ) -> Result<()> {
        if self
            .next
            .is_some_and(|entry| entry.key[1] == u64::from(base))
        {
            self.next = self
                .cursor
                .advance(store, snapshot)?
                .filter(|entry| Some(entry.key[0]) == self.key);
        }
        Ok(())
    }
}

struct Loaded<'a, 'b, S> {
    store: &'a mut S,
    key: GroupKey,
    live: Bitmap<'b>,
    masks: &'a [PageMask],
    terms: &'a [Option<Bitmap<'b>>],
}

impl<S: PageStore> Source for Loaded<'_, '_, S> {
    fn key(&self) -> GroupKey {
        self.key
    }
    fn terms(&self) -> usize {
        self.terms.len()
    }
    fn live_pages(&self) -> PageMask {
        *self.live.pages()
    }
    fn term_pages(&self, term: usize) -> PageMask {
        self.masks[term]
    }
    fn live_offsets(&mut self, page: u8) -> Result<OffsetMask> {
        self.live.offsets(page)
    }
    fn term_offsets(&mut self, term: usize, page: u8) -> Result<(OffsetMask, usize)> {
        let view = self.terms[term].ok_or(Error::InvalidState)?;
        Ok((view.offsets(page)?, view.payload_bytes(page)))
    }
    fn interrupt(&mut self) -> Result<()> {
        self.store.interrupt()
    }
}

struct Program<'a> {
    nodes: [Node; NODES],
    names: [Option<&'a str>; NODES],
    terms: usize,
    len: usize,
    universe: bool,
    seek_terms: u64,
}

fn compile(query: &Query) -> Option<Program<'_>> {
    if query.node_count() > NODES || query.root + 1 != query.node_count() {
        return None;
    }
    let mut result = Program {
        nodes: [Node::All; NODES],
        names: [None; NODES],
        terms: 0,
        len: query.node_count(),
        universe: false,
        seek_terms: 0,
    };
    let mut universe = [false; NODES];
    for (index, node) in query.nodes.iter().enumerate() {
        let (compiled, unbounded) = match &node.kind {
            Kind::None | Kind::Term(_) => {
                let name = match &node.kind {
                    Kind::Term(term) => Some(term.as_str()),
                    _ => None,
                };
                let term = match result.names[..result.terms]
                    .iter()
                    .position(|&old| old == name)
                {
                    Some(term) => term,
                    None => {
                        let term = result.terms;
                        result.names[term] = name;
                        result.terms += 1;
                        term
                    }
                };
                (Node::Term(term), false)
            }
            Kind::And(left, right) => (
                Node::And(*left, *right),
                universe[*left] && universe[*right],
            ),
            Kind::Or(left, right) => (Node::Or(*left, *right), universe[*left] || universe[*right]),
            Kind::Not(child) => (Node::Not(*child), true),
            Kind::Prefix(_) | Kind::Phrase(_) => return None,
        };
        result.nodes[index] = compiled;
        universe[index] = unbounded;
    }
    result.universe = universe[query.root];
    let mut active = 1u64 << query.root;
    for index in (0..result.len).rev() {
        if active & (1 << index) == 0 {
            continue;
        }
        match result.nodes[index] {
            Node::Term(term) => result.seek_terms |= 1 << term,
            Node::And(left, right) | Node::Or(left, right) => {
                active |= (1 << left) | (1 << right);
            }
            // negated children filter offsets but cannot bound the next group.
            Node::Not(_) => {}
            _ => return None,
        }
    }
    Some(result)
}

// each bound is no later than the next possible matching group at or after target.
// and takes the larger bound; or takes the smaller nonempty bound; not cannot skip.
fn next_group<S: PageStore>(
    store: &mut S,
    snapshot: GroupSnapshot,
    program: &Program<'_>,
    terms: &mut [Term],
    groups: &mut Cursor,
    mut target: u32,
) -> Result<Option<u32>> {
    loop {
        store.interrupt()?;
        for (index, term) in terms.iter_mut().enumerate() {
            if program.seek_terms & (1 << index) != 0 {
                term.at(store, snapshot, target)?;
            }
        }
        let mut bounds = [None; NODES];
        for (index, node) in program.nodes[..program.len].iter().enumerate() {
            bounds[index] = match *node {
                Node::Term(term) => terms[term]
                    .next
                    .map(|entry| u32::try_from(entry.key[1]).map_err(|_| Error::InvalidState))
                    .transpose()?,
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
        let Some(mut next) = bounds[program.len - 1] else {
            return Ok(None);
        };
        if program.universe {
            let Some(entry) = groups
                .seek(store, snapshot, [0, u64::from(next)])?
                .filter(|entry| entry.key[0] == 0)
            else {
                return Ok(None);
            };
            next = u32::try_from(entry.key[1]).map_err(|_| Error::InvalidState)?;
        }
        if next < target || !next.is_multiple_of(256) {
            return Err(Error::InvalidState);
        }
        if next == target {
            return Ok(Some(next));
        }
        target = next;
    }
}

/// scans grouped Boolean matches, then a conservative cover of newer complete owners.
/// unsupported syntax, unavailable snapshots and small budgets use the legacy kernel.
/// the host holds a shared structural barrier; only MVCC bitmap consumers are allowed.
///
/// # errors
/// corruption and host failures abort the scan; previously emitted output must be discarded.
/// returned accounting is candidate cardinality, not a visible or distinct SQL count.
pub fn scan_query<S: PageStore>(
    store: &mut S,
    query: &Query,
    memory_bytes: usize,
    mut emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<u64> {
    let Some(program) = compile(query) else {
        return super::super::scan_query_with_recheck(store, query, memory_bytes, emit);
    };
    let required = program.terms * (BITMAP_BYTES + core::mem::size_of::<Term>()) + 128 * 1024;
    if memory_bytes < required {
        return super::super::scan_query_with_recheck(store, query, memory_bytes, emit);
    }
    let meta = load(store, 0, PageKind::Meta)?;
    let Some(snapshot) = meta.grouped_state()?.active else {
        return super::super::scan_query_with_recheck(store, query, memory_bytes, emit);
    };
    let mut terms = Vec::new();
    for name in &program.names[..program.terms] {
        let key = match name {
            Some(name) => {
                let found = find_term(store, &meta, name)?;
                if matches!(query.nodes[query.root].kind, Kind::Term(_)) {
                    let Some((dictionary, reference)) = found else {
                        return Ok(0);
                    };
                    let entry = dictionary.term(reference)?;
                    if entry.head == entry.tail {
                        let first_page = if entry.head == NO_BLOCK {
                            None
                        } else {
                            Some(load_posting(store, entry.head, reference)?)
                        };
                        let postings = match &first_page {
                            Some(page) => page.posting_records()? + 1,
                            None => 1,
                        };
                        if postings <= SPARSE_POSTINGS {
                            return super::super::query::scan_term_entry(
                                store,
                                entry,
                                first_page,
                                |root| emit(root, false),
                            );
                        }
                    }
                    Some((u64::from(reference.page) << 16) | u64::from(reference.offset))
                } else {
                    found.map(|(_, reference)| {
                        (u64::from(reference.page) << 16) | u64::from(reference.offset)
                    })
                }
            }
            None => None,
        };
        if terms.is_empty() {
            terms
                .try_reserve_exact(program.terms)
                .map_err(|_| Error::Allocation)?;
        }
        terms.push(Term {
            key,
            cursor: Cursor::new(),
            next: None,
            initialized: false,
        });
    }
    store.event(super::super::Stage::GroupScan)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(program.terms * BITMAP_BYTES)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(program.terms * BITMAP_BYTES, 0);
    let mut live_bytes = [0; BITMAP_BYTES];
    let mut scratch = QueryScratch::default();
    let mut groups = Cursor::new();
    let mut count = 0u64;
    let mut target = Some(0);
    while let Some(start) = target {
        let Some(base) = next_group(store, snapshot, &program, &mut terms, &mut groups, start)?
        else {
            break;
        };
        // the shared structural barrier keeps this private catalog leaf valid.
        let live_entry = groups
            .seek(store, snapshot, [0, u64::from(base)])?
            .filter(|entry| entry.key == [0, u64::from(base)])
            .ok_or(Error::InvalidState)?;
        let live_value = Value::read(live_entry)?;
        let mut masks = [[0; 4]; NODES];
        let mut entries = [None; NODES];
        for (index, term) in terms.iter_mut().enumerate() {
            entries[index] = term.at(store, snapshot, base)?;
            if let Some(entry) = entries[index] {
                masks[index] = Value::read(entry)?.pages;
            }
        }
        let (candidate_pages, needed) = needed_terms(
            &program.nodes[..program.len],
            live_value.pages,
            &masks[..program.terms],
            &mut scratch,
        )?;
        if candidate_pages != [0; 4] {
            storage::read_bitmap(store, snapshot, live_entry, &mut live_bytes)?;
            for (index, chunk) in bytes.chunks_mut(BITMAP_BYTES).enumerate() {
                if needed & (1 << index) != 0 {
                    storage::read_bitmap(
                        store,
                        snapshot,
                        entries[index].ok_or(Error::InvalidState)?,
                        chunk,
                    )?;
                }
            }
            let mut parsed = [None; NODES];
            for (index, chunk) in bytes.chunks(BITMAP_BYTES).enumerate() {
                if needed & (1 << index) != 0 {
                    let len = Value::read(entries[index].ok_or(Error::InvalidState)?)?.len as usize;
                    parsed[index] = Some(Bitmap::open(&chunk[..len])?);
                }
            }
            let layout = store.layout();
            let key = storage::key(snapshot, base, layout)?;
            let mut source = Loaded {
                store,
                key,
                live: Bitmap::open(&live_bytes[..live_value.len as usize])?,
                masks: &masks[..program.terms],
                terms: &parsed[..program.terms],
            };
            let stats = evaluate_source(
                &mut source,
                &program.nodes[..program.len],
                &mut scratch,
                |block, offsets| {
                    for (word, &value) in offsets.iter().enumerate() {
                        let mut pending = value;
                        while pending != 0 {
                            let bit = word * 64 + pending.trailing_zeros() as usize;
                            pending &= pending - 1;
                            emit(
                                RootTid::new(block, (bit + 1) as u16, layout)
                                    .map_err(|_| Error::InvalidState)?,
                                false,
                            )?;
                        }
                    }
                    Ok(())
                },
            )?;
            count = count
                .checked_add(u64::from(stats.emitted_tids))
                .ok_or(Error::Limit("group candidates"))?;
        }
        for (index, term) in terms.iter_mut().enumerate() {
            if program.seek_terms & (1 << index) != 0 {
                term.advance(store, snapshot, base)?;
            }
        }
        target = base.checked_add(256);
    }
    delta(store, &meta, snapshot, count, emit)
}

fn delta<S: PageStore>(
    store: &mut S,
    meta: &super::super::page::Page,
    snapshot: GroupSnapshot,
    mut count: u64,
    mut emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<u64> {
    let (head, tail) = meta.owner_chain()?;
    if head == NO_BLOCK {
        return Ok(count);
    }
    let mut block = snapshot.after.map_or(head, |after| after.page);
    let mut first = true;
    loop {
        let page = load(store, block, PageKind::Owners)?;
        let start = if first {
            first = false;
            if let Some(after) = snapshot.after {
                if page.owner(after.slot, store.layout())?.reference != after {
                    return Err(Error::InvalidState);
                }
                after.slot + 1
            } else {
                0
            }
        } else {
            0
        };
        for slot in start..page.owner_count()? {
            let owner = page.owner(slot, store.layout())?;
            if owner.reference.incarnation.get() <= snapshot.id.get() {
                return Err(Error::InvalidState);
            }
            if owner.publication == Publication::Published && owner.live {
                emit(owner.root, true)?;
                count = count.checked_add(1).ok_or(Error::Limit("group delta"))?;
            }
        }
        match following(&page, tail)? {
            Some(next) => block = next,
            None => break,
        }
    }
    Ok(count)
}
