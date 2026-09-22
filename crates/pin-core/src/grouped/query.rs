//! Bounded scalar Boolean evaluation inside one immutable generation domain.
//! Page summaries are evaluated before any term offset payload is decoded.

use super::{Bitmap, BitmapKind, SegmentGroup};
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
    interrupt()?;
    validate(segment, terms, program)?;
    let live_pages = segment.live().pages();
    for (index, node) in program.iter().enumerate() {
        scratch.pages[index] = match *node {
            Node::Term(term) => grouped::candidates(
                BitmapOp::Intersection,
                terms[term].pages(),
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
        interrupt()?;
        stats.candidate_pages += 1;
        let live = segment.live().offsets(page)?;
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
                    let bytes = terms[term].payload_bytes(page);
                    if bytes != 0 {
                        stats.term_payloads += 1;
                        stats.term_bytes += bytes as u32;
                    }
                    let offsets = terms[term].offsets(page)?;
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
            emit(segment.key().block(page)?, result)?;
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

fn validate(segment: &SegmentGroup<'_>, terms: &[Bitmap<'_>], program: &[Node]) -> Result<()> {
    if program.is_empty() || program.len() > MAX_NODES || terms.len() > MAX_NODES {
        return Err(Error::Limit("group query nodes"));
    }
    if terms
        .iter()
        .any(|term| term.key() != segment.key() || term.kind() != BitmapKind::Posting)
    {
        return Err(Error::InvalidState);
    }
    for (index, node) in program.iter().enumerate() {
        let valid = match *node {
            Node::Term(term) => term < terms.len(),
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
