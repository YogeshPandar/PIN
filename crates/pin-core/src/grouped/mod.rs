//! Versioned logical segment groups; not PostgreSQL pages or SQL scan storage.
//! Each coordinate has one immutable owner incarnation inside a segment.
//! Full Boolean matches, never partial term masks, may cross segment boundaries.
//! Publication, WAL and adapter gates: docs/g9-grouped-storage.md.

mod bitmap;
mod members;
mod merge;
mod query;

pub use bitmap::{Bitmap, BitmapKind, PageOffsets, encode_bitmap};
pub use members::{Member, Members, SegmentGroup, encode_members, retire};
pub use merge::{MAX_MERGE_SOURCES, MergePlan};
pub use query::{Node, QueryScratch, QueryStats, evaluate};

use crate::codec::bytes::{Reader, Writer};
use crate::error::{Error, Result};
use crate::identity::{Generation, HeapLayout, SegmentId};
use pin_kernels::grouped::PageMask;

pub const VERSION: u16 = 1;
pub const GROUP_PAGES: usize = 256;
pub const HEADER_BYTES: usize = 72;
const MAGIC: &[u8; 4] = b"PNG9";
const MEMBERS: u8 = 3;

/// Identity within an already selected physical index relation.
/// The host never reuses a segment ID within a durable relation generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupKey {
    relation: Generation,
    segment: SegmentId,
    base: u32,
    layout: HeapLayout,
}

impl GroupKey {
    /// Constructs a 256-page-aligned group; the invalid final heap block is absent.
    ///
    /// # Errors
    /// Rejects an unaligned base; coordinates are checked independently.
    pub fn new(
        relation: Generation,
        segment: SegmentId,
        base: u32,
        layout: HeapLayout,
    ) -> Result<Self> {
        if base & 255 != 0 {
            return Err(Error::InvalidParameters);
        }
        Ok(Self {
            relation,
            segment,
            base,
            layout,
        })
    }

    pub const fn relation(self) -> Generation {
        self.relation
    }

    pub const fn segment(self) -> SegmentId {
        self.segment
    }

    pub const fn base(self) -> u32 {
        self.base
    }

    pub const fn layout(self) -> HeapLayout {
        self.layout
    }

    fn block(self, page: u8) -> Result<u32> {
        let block = self.base | u32::from(page);
        if block == u32::MAX {
            return Err(Error::InvalidState);
        }
        Ok(block)
    }
}

#[derive(Clone, Copy)]
struct Header {
    key: GroupKey,
    kind: u8,
    count: u32,
    mask: PageMask,
}

impl Header {
    fn read(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != MAGIC || reader.u16()? != VERSION {
            return Err(Error::InvalidState);
        }
        let kind = reader.u8()?;
        if !matches!(kind, 1..=3) || reader.u8()? != 0 {
            return Err(Error::InvalidState);
        }
        if usize::try_from(reader.u32()?).map_err(|_| Error::InvalidState)? != bytes.len() {
            return Err(Error::InvalidState);
        }
        let base = reader.u32()?;
        let relation = Generation::new(reader.u64()?).map_err(|_| Error::InvalidState)?;
        let segment = SegmentId::new(reader.u64()?).map_err(|_| Error::InvalidState)?;
        let layout = HeapLayout::new(reader.u16()?).map_err(|_| Error::InvalidState)?;
        let pages = reader.u16()?;
        let count = reader.u32()?;
        let mut mask = [0; 4];
        for word in &mut mask {
            *word = reader.u64()?;
        }
        let key = GroupKey::new(relation, segment, base, layout)?;
        if pages != page_count(&mask) || (base == u32::MAX - 255 && mask[3] >> 63 != 0) {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            key,
            kind,
            count,
            mask,
        })
    }

    fn write(self, writer: &mut Writer<'_>, length: usize) -> Result<()> {
        writer.put(MAGIC)?;
        writer.u16(VERSION)?;
        writer.u8(self.kind)?;
        writer.u8(0)?;
        writer.u32(u32::try_from(length).map_err(|_| Error::Limit("group bytes"))?)?;
        writer.u32(self.key.base)?;
        writer.u64(self.key.relation.get())?;
        writer.u64(self.key.segment.get())?;
        writer.u16(self.key.layout.max_offset())?;
        writer.u16(page_count(&self.mask))?;
        writer.u32(self.count)?;
        for word in self.mask {
            writer.u64(word)?;
        }
        Ok(())
    }
}

fn page_count(mask: &PageMask) -> u16 {
    mask.iter().map(|word| word.count_ones() as u16).sum()
}

fn contains(mask: &PageMask, page: u8) -> bool {
    mask[usize::from(page) / 64] & (1 << (page % 64)) != 0
}

fn insert(mask: &mut PageMask, page: u8) {
    mask[usize::from(page) / 64] |= 1 << (page % 64);
}
