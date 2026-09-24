//! Checked logical compaction over consistent private segment snapshots.
//! The host owns source pinning, fresh identity reservation and WAL publication.

use super::bitmap::encode_with;
use super::{Bitmap, BitmapKind, GroupKey, HEADER_BYTES, Header, MEMBERS, Member, SegmentGroup};
use crate::codec::bytes::Writer;
use crate::error::{Error, Result};
use pin_kernels::grouped::{OffsetMask, PageMask, Pages};

pub const MAX_MERGE_SOURCES: usize = 16;

/// A validated live-owner merge; no target bytes or source pointers are retained
/// beyond their Rust borrows. Retired occupants never enter target membership.
pub struct MergePlan<'a, 'data> {
    key: GroupKey,
    sources: &'a [&'a SegmentGroup<'data>],
    mask: PageMask,
    count: u32,
}

impl<'a, 'data> MergePlan<'a, 'data> {
    /// Checks fresh identity and rejects live generations sharing one coordinate.
    /// The caller must supply complete documents, not partial per-term flushes.
    ///
    /// # Errors
    /// Rejects foreign groups, repeated sources, identity reuse and live conflicts.
    pub fn new(
        key: GroupKey,
        sources: &'a [&'a SegmentGroup<'data>],
        mut interrupt: impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        if sources.len() > MAX_MERGE_SOURCES {
            return Err(Error::Limit("group merge sources"));
        }
        for (index, source) in sources.iter().enumerate() {
            let source_key = source.key();
            if source_key.base() != key.base()
                || source_key.relation() != key.relation()
                || source_key.layout() != key.layout()
                || source_key.segment() == key.segment()
                || sources[..index]
                    .iter()
                    .any(|prior| prior.key() == source_key)
            {
                return Err(Error::InvalidState);
            }
        }
        let mut count = 0;
        let mut mask = [0; 4];
        visit(sources, &mut interrupt, |member| {
            count += 1;
            super::insert(&mut mask, member.root.block() as u8);
            Ok(())
        })?;
        Ok(Self {
            key,
            sources,
            mask,
            count,
        })
    }

    pub const fn key(&self) -> GroupKey {
        self.key
    }

    pub const fn len(&self) -> u32 {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Encodes one root/incarnation pair for each distinct surviving owner.
    ///
    /// # Errors
    /// Rejects insufficient capacity before writing any target bytes.
    /// Cancellation may leave partial output, which must never be published.
    pub fn encode_members(
        &self,
        output: &mut [u8],
        mut interrupt: impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        interrupt()?;
        let length = HEADER_BYTES + self.count as usize * 16;
        if output.len() < length {
            return Err(Error::Limit("merged membership output"));
        }
        let mut writer = Writer::new(&mut output[..length]);
        Header {
            key: self.key,
            kind: MEMBERS,
            count: self.count,
            mask: self.mask,
        }
        .write(&mut writer, length)?;
        visit(self.sources, &mut interrupt, |member| {
            writer.u32(member.root.block())?;
            writer.u16(member.root.offset())?;
            writer.u16(0)?;
            writer.u64(member.incarnation.get())?;
            Ok(())
        })?;
        Ok(length)
    }

    /// Creates fresh liveness containing exactly the merged live membership.
    ///
    /// # Errors
    /// Rejects insufficient capacity before writing any target bytes.
    /// Cancellation may leave partial output, which must never be published.
    pub fn encode_liveness(
        &self,
        output: &mut [u8],
        mut interrupt: impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        interrupt()?;
        encode_with(self.key, BitmapKind::Liveness, self.mask, output, |page| {
            interrupt()?;
            let mut offsets = [0; 8];
            for source in self.sources {
                for (output, live) in offsets.iter_mut().zip(source.live().offsets(page)?) {
                    *output |= live;
                }
            }
            Ok(offsets)
        })
    }

    /// Merges one term across sources after masking each source's own liveness.
    /// `terms[i]` belongs to `sources[i]`; None means that complete source lacks
    /// this term. Filtering after union would resurrect retired reused TIDs.
    ///
    /// # Errors
    /// Rejects missing source slots, mismatched identities, malformed payloads
    /// and insufficient capacity before writing any target bytes.
    /// Cancellation may leave partial output, which must never be published.
    pub fn encode_posting(
        &self,
        terms: &[Option<Bitmap<'_>>],
        output: &mut [u8],
        mut interrupt: impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        interrupt()?;
        if terms.len() != self.sources.len() {
            return Err(Error::InvalidParameters);
        }
        let mut pages = [0; 4];
        for (source, term) in self.sources.iter().zip(terms) {
            if let Some(term) = term {
                if term.key() != source.key() || term.kind() != BitmapKind::Posting {
                    return Err(Error::InvalidState);
                }
                interrupt()?;
                term.validate_all()?;
                for (pages, source_pages) in pages.iter_mut().zip(term.pages()) {
                    *pages |= source_pages;
                }
            }
        }
        let mut mask = [0; 4];
        for page in Pages::new(pages) {
            interrupt()?;
            if self
                .term_offsets(terms, page)?
                .iter()
                .any(|&word| word != 0)
            {
                super::insert(&mut mask, page);
            }
        }
        encode_with(self.key, BitmapKind::Posting, mask, output, |page| {
            interrupt()?;
            self.term_offsets(terms, page)
        })
    }

    fn term_offsets(&self, terms: &[Option<Bitmap<'_>>], page: u8) -> Result<OffsetMask> {
        let mut result = [0; 8];
        for (source, term) in self.sources.iter().zip(terms) {
            if let Some(term) = term {
                let offsets = term.offsets(page)?;
                let live = source.live().offsets(page)?;
                for ((output, posting), live) in result.iter_mut().zip(offsets).zip(live) {
                    *output |= posting & live;
                }
            }
        }
        Ok(result)
    }
}

struct Cursor<'a, 'data> {
    source: &'a SegmentGroup<'data>,
    index: usize,
    page: Option<u8>,
    live: OffsetMask,
    current: Option<Member>,
}

impl Cursor<'_, '_> {
    fn advance(&mut self, interrupt: &mut impl FnMut() -> Result<()>) -> Result<()> {
        self.current = None;
        while self.index < self.source.members().len() {
            if self.index & 127 == 0 {
                interrupt()?;
            }
            let member = self.source.members().get(self.index)?;
            self.index += 1;
            let page = member.root.block() as u8;
            if self.page != Some(page) {
                self.live = self.source.live().offsets(page)?;
                self.page = Some(page);
            }
            let bit = usize::from(member.root.offset() - 1);
            if self.live[bit / 64] & (1 << (bit % 64)) != 0 {
                self.current = Some(member);
                break;
            }
        }
        Ok(())
    }
}

// each coordinate is emitted once, and all live copies must name one owner.
fn visit(
    sources: &[&SegmentGroup<'_>],
    interrupt: &mut impl FnMut() -> Result<()>,
    mut emit: impl FnMut(Member) -> Result<()>,
) -> Result<()> {
    interrupt()?;
    let mut cursors: [Option<Cursor<'_, '_>>; MAX_MERGE_SOURCES] = std::array::from_fn(|_| None);
    for (slot, source) in cursors.iter_mut().zip(sources) {
        let mut cursor = Cursor {
            source,
            index: 0,
            page: None,
            live: [0; 8],
            current: None,
        };
        cursor.advance(interrupt)?;
        *slot = Some(cursor);
    }
    loop {
        let next = cursors[..sources.len()]
            .iter()
            .flatten()
            .filter_map(|cursor| cursor.current)
            .min_by_key(|member| member.root);
        let Some(member) = next else {
            return Ok(());
        };
        for cursor in cursors[..sources.len()].iter_mut().flatten() {
            let Some(current) = cursor.current else {
                continue;
            };
            if current.root != member.root {
                continue;
            }
            if current.incarnation != member.incarnation {
                return Err(Error::InvalidState);
            }
            cursor.advance(interrupt)?;
        }
        emit(member)?;
    }
}
