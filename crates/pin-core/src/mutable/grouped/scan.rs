//! page-group pruning over a complete snapshot plus a term-addressed write frontier.
//! bitmap output remains subject to PostgreSQL heap visibility and rechecks.

use super::super::page::{CatalogEntry, GroupSnapshot, NO_BLOCK, PageKind};
use super::super::{PageStore, find_term, load, load_posting};
use super::storage::{self, BITMAP_BYTES, Cursor, Value};
use crate::error::{Error, Result};
use crate::grouped::{
    Bitmap, GroupKey, Node, QueryScratch, QueryStats, Source, evaluate_source, needed_terms,
};
use crate::identity::{HeapLayout, RootTid};
use crate::query::{Kind, Query};
use pin_kernels::grouped::{OffsetMask, PageMask};

#[path = "frontier.rs"]
mod frontier;

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

fn reset_terms(terms: &mut [Term]) {
    for term in terms {
        term.cursor = Cursor::new();
        term.next = None;
        term.initialized = false;
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one reusable query workspace serves every immutable segment"
)]
fn scan_snapshot<S: PageStore, T: ExactSink>(
    store: &mut S,
    snapshot: GroupSnapshot,
    program: &Program<'_>,
    terms: &mut [Term],
    bytes: &mut [u8],
    live_bytes: &mut [u8; BITMAP_BYTES],
    scratch: &mut QueryScratch,
    sink: &mut T,
) -> Result<u64> {
    reset_terms(terms);
    let mut groups = Cursor::new();
    let mut count = 0u64;
    let mut target = Some(0);
    while let Some(start) = target {
        let Some(base) = next_group(store, snapshot, program, terms, &mut groups, start)? else {
            break;
        };
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
            scratch,
        )?;
        if candidate_pages != [0; 4] {
            let (_, live) = storage::read_bitmap_view(store, snapshot, live_entry, live_bytes)?;
            let mut parsed = [None; NODES];
            for (index, chunk) in bytes.chunks_mut(BITMAP_BYTES).enumerate() {
                if needed & (1 << index) != 0 {
                    let (_, view) = storage::read_bitmap_view(
                        store,
                        snapshot,
                        entries[index].ok_or(Error::InvalidState)?,
                        chunk,
                    )?;
                    parsed[index] = Some(view);
                }
            }
            let layout = store.layout();
            let key = storage::key(snapshot, base, layout)?;
            let mut source = Loaded {
                store,
                key,
                live,
                masks: &masks[..program.terms],
                terms: &parsed[..program.terms],
            };
            let stats = evaluate_source(
                &mut source,
                &program.nodes[..program.len],
                scratch,
                |block, offsets| sink.page(layout, block, offsets),
            )?;
            sink.work(stats)?;
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
    Ok(count)
}

/// consumes exact predicate membership, not a PostgreSQL visibility proof.
/// page masks belong to one protected snapshot; suffix roots remain owner-qualified.
/// the default expands offsets only for consumers that need individual TIDs.
pub trait ExactSink {
    /// records work from one sealed group, excluding sparse and frontier payloads.
    ///
    /// # errors
    /// accounting failure invalidates the whole scan like a row callback failure.
    fn work(&mut self, _stats: QueryStats) -> Result<()> {
        Ok(())
    }

    /// accepts one predicate-qualified root from the sparse or mutable path.
    ///
    /// # errors
    /// a consumer failure aborts the entire scan and invalidates earlier output.
    fn tid(&mut self, root: RootTid) -> Result<()>;

    /// accepts one nonempty, liveness-masked heap-page result.
    ///
    /// # errors
    /// invalid offsets or a consumer failure abort the entire scan.
    fn page(&mut self, layout: HeapLayout, block: u32, offsets: &[u64; 8]) -> Result<()> {
        for (word, &value) in offsets.iter().enumerate() {
            let mut pending = value;
            while pending != 0 {
                let bit = word * 64 + pending.trailing_zeros() as usize;
                pending &= pending - 1;
                self.tid(
                    RootTid::new(block, (bit + 1) as u16, layout)
                        .map_err(|_| Error::InvalidState)?,
                )?;
            }
        }
        Ok(())
    }
}

struct ScalarSink<'a, F>(&'a mut F);

impl<F: FnMut(RootTid, bool) -> Result<()>> ExactSink for ScalarSink<'_, F> {
    fn tid(&mut self, root: RootTid) -> Result<()> {
        self.0(root, false)
    }
}

/// reports syntactic eligibility only; storage and memory are checked by scan_exact.
#[must_use]
pub fn supports_exact(query: &Query) -> bool {
    compile(query).is_some()
}

/// scans grouped matches, retaining the legacy bitmap fallback unchanged.
/// the host holds its structural reader barrier and supplies an MVCC bitmap sink.
///
/// # errors
/// corruption or host failures invalidate all previously emitted candidates.
pub fn scan_query<S: PageStore>(
    store: &mut S,
    query: &Query,
    memory_bytes: usize,
    mut emit: impl FnMut(RootTid, bool) -> Result<()>,
) -> Result<u64> {
    match scan_exact(store, query, memory_bytes, &mut ScalarSink(&mut emit))? {
        Some(count) => Ok(count),
        None => super::super::scan_query_with_recheck(store, query, memory_bytes, emit),
    }
}

/// scans exact grouped matches without expanding sealed page masks into TIDs.
/// sparse and newer-owner paths retain their bounded scalar representation.
/// returns None before any emission for unsupported syntax, snapshot or memory budget.
/// the host holds the structural barrier. Visibility-eliding consumers additionally
/// prevent owner retirement/reuse from before the first read through the last decision.
/// one live owner per heap root is a host lifecycle invariant, not a deduplication here.
///
/// # errors
/// corruption and host failures abort; the consumer must discard all partial output.
/// Some contains candidate cardinality, never a visible SQL count by itself.
pub fn scan_exact<S: PageStore, T: ExactSink>(
    store: &mut S,
    query: &Query,
    memory_bytes: usize,
    sink: &mut T,
) -> Result<Option<u64>> {
    let Some(program) = compile(query) else {
        return Ok(None);
    };
    let required = program.terms * (BITMAP_BYTES + core::mem::size_of::<Term>()) + 128 * 1024;
    if memory_bytes < required.max(frontier::memory(program.terms)) {
        return Ok(None);
    }
    let meta = load(store, 0, PageKind::Meta)?;
    let state = meta.grouped_state()?;
    let Some(active) = state.active else {
        return Ok(None);
    };
    let latest = state.latest().ok_or(Error::InvalidState)?;
    let mut terms = Vec::new();
    let mut captured = [None; NODES];
    for (index, name) in program.names[..program.terms].iter().enumerate() {
        let key = match name {
            Some(name) => {
                let found = find_term(store, &meta, name)?;
                if let Some((dictionary, reference)) = &found {
                    captured[index] = Some(frontier::CapturedTerm::new(dictionary.term(*reference)?));
                }
                if matches!(query.nodes[query.root].kind, Kind::Term(_)) {
                    let Some((dictionary, reference)) = found else {
                        return Ok(Some(0));
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
                                |root| sink.tid(root),
                            )
                            .map(Some);
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
    let mut count = scan_snapshot(
        store,
        active,
        &program,
        &mut terms,
        &mut bytes,
        &mut live_bytes,
        &mut scratch,
        sink,
    )?;
    for delta in state.deltas.iter().flatten() {
        let segment = scan_snapshot(
            store,
            delta.snapshot,
            &program,
            &mut terms,
            &mut bytes,
            &mut live_bytes,
            &mut scratch,
            sink,
        )?;
        count = count
            .checked_add(segment)
            .ok_or(Error::Limit("group candidates"))?;
    }
    drop(bytes);
    drop(terms);
    let frontier = frontier::scan(
        store,
        &program,
        &captured[..program.terms],
        &meta,
        latest,
        memory_bytes,
        |root, recheck| {
            if recheck {
                return Err(Error::InvalidState);
            }
            sink.tid(root)
        },
    )?;
    count
        .checked_add(frontier)
        .map(Some)
        .ok_or(Error::Limit("group candidates"))
}
