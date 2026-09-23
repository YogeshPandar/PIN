//! Bounded scalar Boolean evaluation inside one immutable generation domain.
//! Page summaries are evaluated before any term offset payload is decoded.

use super::{Bitmap, BitmapKind, GroupKey, SegmentGroup};
use crate::error::{Error, Result};
use pin_kernels::BitmapOp;
use pin_kernels::grouped::{self, OffsetMask, PageMask, Pages};

pub const MAX_NODES: usize = 64;

/// A topologically ordered program; child indices must precede their parent.
/// `All` and `Not` are relative to this segment's live membership, not all TIDs.
#[derive(Clone, Copy, Debug)]
pub enum Node {
    Term(usize),
    All,
    And(usize, usize),
    Or(usize, usize),
    Difference(usize, usize),
    Not(usize),
}

/// Caller-owned fixed scratch, reusable across segments without allocation.
pub struct QueryScratch {
    pages: [PageMask; MAX_NODES],
    offsets: [OffsetMask; MAX_NODES],
}

impl Default for QueryScratch {
    fn default() -> Self {
        Self {
            pages: [[0; 4]; MAX_NODES],
            offsets: [[0; 8]; MAX_NODES],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueryStats {
    pub candidate_pages: u16,
    pub live_pages: u16,
    pub term_payloads: u32,
    pub term_bytes: u32,
    pub emitted_tids: u32,
}

/// Emits complete Boolean matches as heap-ordered page/offset masks.
/// Terms must describe complete documents from this exact segment identity.
/// Only completed matches may be unioned with matches from other segments.
/// The host checks heap visibility and discards all output if a callback fails.
///
/// # Errors
/// Rejects oversized/invalid programs and identity mismatches before emission.
/// Payload corruption or callback errors can occur after earlier emitted pages.
pub fn evaluate(
    segment: &SegmentGroup<'_>,
    terms: &[Bitmap<'_>],
    program: &[Node],
    scratch: &mut QueryScratch,
    mut interrupt: impl FnMut() -> Result<()>,
    mut emit: impl FnMut(u32, &OffsetMask) -> Result<()>,
) -> Result<QueryStats> {
    if terms.iter().any(|term| term.key() != segment.key() || term.kind() != BitmapKind::Posting) {
        return Err(Error::InvalidState);
    }
    let mut source = MemorySource { segment, terms, interrupt: &mut interrupt };
    evaluate_source(&mut source, program, scratch, &mut emit)
}

// sources bind all records to one published immutable membership before decoding.
pub(crate) trait Source {
    fn key(&self) -> GroupKey;
    fn terms(&self) -> usize;
    fn live_pages(&self) -> PageMask;
    fn term_pages(&self, term: usize) -> PageMask;
    fn live_offsets(&mut self, page: u8) -> Result<OffsetMask>;
    fn term_offsets(&mut self, term: usize, page: u8) -> Result<(OffsetMask, usize)>;
    fn interrupt(&mut self) -> Result<()>;
}

struct MemorySource<'a, 'b, I> {
    segment: &'a SegmentGroup<'b>,
    terms: &'a [Bitmap<'b>],
    interrupt: I,
}

impl<I: FnMut() -> Result<()>> Source for MemorySource<'_, '_, I> {
    fn key(&self) -> GroupKey { self.segment.key() }
    fn terms(&self) -> usize { self.terms.len() }
    fn live_pages(&self) -> PageMask { *self.segment.live().pages() }
    fn term_pages(&self, term: usize) -> PageMask { *self.terms[term].pages() }
    fn live_offsets(&mut self, page: u8) -> Result<OffsetMask> { self.segment.live().offsets(page) }
    fn term_offsets(&mut self, term: usize, page: u8) -> Result<(OffsetMask, usize)> {
        Ok((self.terms[term].offsets(page)?, self.terms[term].payload_bytes(page)))
    }
    fn interrupt(&mut self) -> Result<()> { (self.interrupt)() }
}

pub(crate) fn evaluate_source(
    source: &mut impl Source, program: &[Node], scratch: &mut QueryScratch,
    mut emit: impl FnMut(u32, &OffsetMask) -> Result<()>,
) -> Result<QueryStats> {
    source.interrupt()?;
    validate_program(source.terms(), program)?;
    let live_pages = &source.live_pages();
    for (index, node) in program.iter().enumerate() {
        scratch.pages[index] = match *node {
            Node::Term(term) => grouped::candidates(
                BitmapOp::Intersection,
                &source.term_pages(term),
                live_pages,
                live_pages,
            ),
            Node::All | Node::Not(_) => *live_pages,
            Node::And(left, right) | Node::Or(left, right) | Node::Difference(left, right) => {
                grouped::candidates(
                    operation(*node),
                    &scratch.pages[left],
                    &scratch.pages[right],
                    live_pages,
                )
            }
        };
    }
    let mut stats = QueryStats::default();
    for page in Pages::new(scratch.pages[program.len() - 1]) {
        source.interrupt()?;
        stats.candidate_pages += 1;
        let live = source.live_offsets(page)?;
        if live.iter().all(|&word| word == 0) {
            continue;
        }
        stats.live_pages += 1;
        let needed = dependencies(program, &scratch.pages, page);
        for (index, node) in program.iter().enumerate() {
            if needed & (1 << index) == 0 {
                continue;
            }
            if !super::contains(&scratch.pages[index], page) {
                scratch.offsets[index] = [0; 8];
                continue;
            }
            scratch.offsets[index] = match *node {
                Node::Term(term) => {
                    let (offsets, bytes) = source.term_offsets(term, page)?;
                    if bytes != 0 {
                        stats.term_payloads += 1;
                        stats.term_bytes += bytes as u32;
                    }
                    grouped::offsets(BitmapOp::Intersection, &offsets, &live, &live)
                }
                Node::All => live,
                Node::Not(child) => {
                    grouped::offsets(BitmapOp::Difference, &live, &scratch.offsets[child], &live)
                }
                Node::And(left, right) | Node::Or(left, right) | Node::Difference(left, right) => {
                    grouped::offsets(
                        operation(*node),
                        &scratch.offsets[left],
                        &scratch.offsets[right],
                        &live,
                    )
                }
            };
        }
        let result = &scratch.offsets[program.len() - 1];
        let count: u32 = result.iter().map(|word| word.count_ones()).sum();
        if count != 0 {
            emit(source.key().block(page)?, result)?;
            stats.emitted_tids += count;
        }
    }
    Ok(stats)
}

// empty subexpressions supply zero without decoding their descendants.
fn dependencies(program: &[Node], pages: &[PageMask; MAX_NODES], page: u8) -> u64 {
    let mut needed = 1u64 << (program.len() - 1);
    for (index, node) in program.iter().enumerate().rev() {
        if needed & (1 << index) == 0 || !super::contains(&pages[index], page) {
            continue;
        }
        match *node {
            Node::Not(child) => needed |= 1 << child,
            Node::And(left, right) | Node::Or(left, right) | Node::Difference(left, right) => {
                needed |= (1 << left) | (1 << right);
            }
            Node::Term(_) | Node::All => {}
        }
    }
    needed
}

fn operation(node: Node) -> BitmapOp {
    match node {
        Node::And(..) => BitmapOp::Intersection,
        Node::Or(..) => BitmapOp::Union,
        Node::Difference(..) => BitmapOp::Difference,
        _ => unreachable!("only binary nodes select an operation"),
    }
}

fn validate_program(terms: usize, program: &[Node]) -> Result<()> {
    if program.is_empty() || program.len() > MAX_NODES || terms > MAX_NODES {
        return Err(Error::Limit("group query nodes"));
    }
    for (index, node) in program.iter().enumerate() {
        let valid = match *node {
            Node::Term(term) => term < terms,
            Node::All => true,
            Node::Not(child) => child < index,
            Node::And(left, right) | Node::Or(left, right) | Node::Difference(left, right) => {
                left < index && right < index
            }
        };
        if !valid {
            return Err(Error::InvalidParameters);
        }
    }
    Ok(())
}

// resolves demand using summaries alone, before the adapter reads bitmap extents.
pub(crate) fn needed_terms(
    program: &[Node], live: PageMask, terms: &[PageMask], scratch: &mut QueryScratch,
) -> Result<(PageMask, u64)> {
    validate_program(terms.len(), program)?;
    for (index, node) in program.iter().enumerate() {
        scratch.pages[index] = match *node {
            Node::Term(term) => grouped::candidates(BitmapOp::Intersection, &terms[term], &live, &live),
            Node::All | Node::Not(_) => live,
            Node::And(left, right) | Node::Or(left, right) | Node::Difference(left, right) =>
                grouped::candidates(operation(*node), &scratch.pages[left], &scratch.pages[right], &live),
        };
    }
    let pages = scratch.pages[program.len() - 1];
    let mut terms = 0u64;
    for page in Pages::new(pages) {
        let needed = dependencies(program, &scratch.pages, page);
        for (index, node) in program.iter().enumerate() {
            if needed & (1 << index) != 0 && super::contains(&scratch.pages[index], page) {
                if let Node::Term(term) = *node { terms |= 1 << term; }
            }
        }
    }
    Ok((pages, terms))
}
