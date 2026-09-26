//! candidate primary format with independently addressable heap-page membership.
//! this codec does not establish publication, liveness, or sql visibility.

use crate::codec::bytes::{Reader, Writer};
use crate::error::{Error, Result};
use crate::grouped::GroupKey;
use crate::mutable::page::{CAPACITY, NO_BLOCK};
use crate::mutable::{PageStore, page::PageKind};
use pin_kernels::grouped::OffsetMask;

const MAGIC: &[u8; 4] = b"PNP2";
const VERSION: u16 = 1;
const HEADER: usize = 76;
const ENTRY: usize = 12;
pub const MAX_DIRECTORY_BYTES: usize = HEADER + 256 * ENTRY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ContainerKind {
    Sparse = 1,
    Dense = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub block: u32,
    pub offset: u16,
    pub len: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageDescriptor {
    pub page: u8,
    pub kind: ContainerKind,
    pub count: u16,
    pub extent: Extent,
}

impl PageDescriptor {
    fn valid(self, max_offset: u16) -> bool {
        let dense_len = usize::from(max_offset).div_ceil(8);
        let length = usize::from(self.extent.len);
        self.count != 0
            && self.count <= max_offset
            && self.extent.block != 0
            && self.extent.block != NO_BLOCK
            && usize::from(self.extent.offset) + length <= CAPACITY
            && match self.kind {
                ContainerKind::Sparse => {
                    length == usize::from(self.count) * 2 && length < dense_len
                }
                ContainerKind::Dense => length == dense_len,
            }
    }
}

/// a directory fits in one private postgres page even with all 256 heap pages present.
pub fn encode_directory(key: GroupKey, term: u64, entries: &[PageDescriptor]) -> Result<Vec<u8>> {
    if term == 0 || entries.len() > 256 {
        return Err(Error::InvalidParameters);
    }
    let length = HEADER + entries.len() * ENTRY;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(length, 0);
    let mut mask = [0u64; 4];
    let mut previous = None;
    for &entry in entries {
        if !entry.valid(key.layout().max_offset())
            || (key.base() == u32::MAX - 255 && entry.page == 255)
            || previous.is_some_and(|old| old >= entry.page)
        {
            return Err(Error::InvalidParameters);
        }
        previous = Some(entry.page);
        mask[usize::from(entry.page) / 64] |= 1 << (entry.page % 64);
    }
    let mut writer = Writer::new(&mut bytes);
    writer.put(MAGIC)?;
    writer.u16(VERSION)?;
    writer.u16(0)?;
    writer.u32(length as u32)?;
    writer.u32(key.base())?;
    writer.u16(key.layout().max_offset())?;
    writer.u16(entries.len() as u16)?;
    writer.u64(key.relation().get())?;
    writer.u64(key.segment().get())?;
    writer.u64(term)?;
    for word in mask {
        writer.u64(word)?;
    }
    for entry in entries {
        writer.u8(entry.page)?;
        writer.u8(entry.kind as u8)?;
        writer.u16(entry.count)?;
        writer.u32(entry.extent.block)?;
        writer.u16(entry.extent.offset)?;
        writer.u16(entry.extent.len)?;
    }
    debug_assert_eq!(writer.len(), length);
    Ok(bytes)
}

#[derive(Clone, Copy)]
pub struct Directory<'a> {
    bytes: &'a [u8],
    key: GroupKey,
    term: u64,
    mask: [u64; 4],
}

impl<'a> Directory<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        if !(HEADER..=MAX_DIRECTORY_BYTES).contains(&bytes.len()) {
            return Err(Error::InvalidState);
        }
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != MAGIC || reader.u16()? != VERSION || reader.u16()? != 0 {
            return Err(Error::InvalidState);
        }
        let length = reader.u32()? as usize;
        let base = reader.u32()?;
        let layout =
            crate::identity::HeapLayout::new(reader.u16()?).map_err(|_| Error::InvalidState)?;
        let count = usize::from(reader.u16()?);
        let relation =
            crate::identity::Generation::new(reader.u64()?).map_err(|_| Error::InvalidState)?;
        let segment =
            crate::identity::SegmentId::new(reader.u64()?).map_err(|_| Error::InvalidState)?;
        let term = reader.u64()?;
        let mut mask = [0; 4];
        for word in &mut mask {
            *word = reader.u64()?;
        }
        if term == 0 || length != bytes.len() || length != HEADER + count * ENTRY {
            return Err(Error::InvalidState);
        }
        let key = GroupKey::new(relation, segment, base, layout)?;
        let mut seen = [0u64; 4];
        let mut previous = None;
        for _ in 0..count {
            let page = reader.u8()?;
            let kind = match reader.u8()? {
                1 => ContainerKind::Sparse,
                2 => ContainerKind::Dense,
                _ => return Err(Error::InvalidState),
            };
            let descriptor = PageDescriptor {
                page,
                kind,
                count: reader.u16()?,
                extent: Extent {
                    block: reader.u32()?,
                    offset: reader.u16()?,
                    len: reader.u16()?,
                },
            };
            if !descriptor.valid(layout.max_offset()) || previous.is_some_and(|old| old >= page) {
                return Err(Error::InvalidState);
            }
            previous = Some(page);
            seen[usize::from(page) / 64] |= 1 << (page % 64);
        }
        reader.finish()?;
        if seen != mask || (base == u32::MAX - 255 && mask[3] >> 63 != 0) {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            bytes,
            key,
            term,
            mask,
        })
    }

    pub const fn key(self) -> GroupKey {
        self.key
    }

    pub const fn term(self) -> u64 {
        self.term
    }

    pub const fn pages(self) -> [u64; 4] {
        self.mask
    }

    pub fn page(self, page: u8) -> Result<Option<PageDescriptor>> {
        let word = usize::from(page) / 64;
        let bit = page % 64;
        if self.mask[word] & (1 << bit) == 0 {
            return Ok(None);
        }
        let prior: usize = self.mask[..word]
            .iter()
            .map(|mask| mask.count_ones() as usize)
            .sum();
        let rank = prior + (self.mask[word] & ((1u64 << bit) - 1)).count_ones() as usize;
        let start = HEADER + rank * ENTRY;
        let mut reader = Reader::new(&self.bytes[start..start + ENTRY]);
        let stored_page = reader.u8()?;
        let kind = match reader.u8()? {
            1 => ContainerKind::Sparse,
            2 => ContainerKind::Dense,
            _ => return Err(Error::InvalidState),
        };
        let descriptor = PageDescriptor {
            page: stored_page,
            kind,
            count: reader.u16()?,
            extent: Extent {
                block: reader.u32()?,
                offset: reader.u16()?,
                len: reader.u16()?,
            },
        };
        if stored_page != page {
            return Err(Error::InvalidState);
        }
        Ok(Some(descriptor))
    }

    /// calls the host only for the selected heap page's payload extent.
    pub fn offsets(
        self,
        page: u8,
        mut fetch: impl FnMut(Extent, &mut [u8]) -> Result<()>,
    ) -> Result<Option<OffsetMask>> {
        let Some(descriptor) = self.page(page)? else {
            return Ok(None);
        };
        let mut bytes = [0u8; 64];
        let length = usize::from(descriptor.extent.len);
        fetch(descriptor.extent, &mut bytes[..length])?;
        let mut offsets = [0u64; 8];
        match descriptor.kind {
            ContainerKind::Sparse => {
                let mut previous = 0;
                for chunk in bytes[..length].as_chunks::<2>().0 {
                    let offset = u16::from_le_bytes([chunk[0], chunk[1]]);
                    if offset <= previous || offset > self.key.layout().max_offset() {
                        return Err(Error::InvalidState);
                    }
                    previous = offset;
                    let bit = usize::from(offset - 1);
                    offsets[bit / 64] |= 1 << (bit % 64);
                }
            }
            ContainerKind::Dense => {
                for (index, &byte) in bytes[..length].iter().enumerate() {
                    offsets[index / 8] |= u64::from(byte) << ((index % 8) * 8);
                }
                let max = usize::from(self.key.layout().max_offset());
                if max % 64 != 0 && offsets[max / 64] >> (max % 64) != 0 {
                    return Err(Error::InvalidState);
                }
            }
        }
        if offsets.iter().map(|word| word.count_ones()).sum::<u32>() != u32::from(descriptor.count)
        {
            return Err(Error::InvalidState);
        }
        Ok(Some(offsets))
    }
}

/// encodes sorted one-based heap offsets, choosing the smaller page container.
pub fn encode_offsets(max_offset: u16, offsets: &[u16]) -> Result<(ContainerKind, Vec<u8>)> {
    if offsets.is_empty()
        || max_offset == 0
        || max_offset > 512
        || offsets.len() > usize::from(max_offset)
    {
        return Err(Error::InvalidParameters);
    }
    let mut previous = 0;
    for &offset in offsets {
        if offset <= previous || offset > max_offset {
            return Err(Error::InvalidParameters);
        }
        previous = offset;
    }
    let dense_len = usize::from(max_offset).div_ceil(8);
    let sparse = offsets.len() * 2 < dense_len;
    let kind = if sparse {
        ContainerKind::Sparse
    } else {
        ContainerKind::Dense
    };
    let len = if sparse { offsets.len() * 2 } else { dense_len };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(len, 0);
    if sparse {
        for (chunk, &offset) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(offsets) {
            chunk.copy_from_slice(&offset.to_le_bytes());
        }
    } else {
        for &offset in offsets {
            let bit = usize::from(offset - 1);
            bytes[bit / 8] |= 1 << (bit % 8);
        }
    }
    Ok((kind, bytes))
}

/// reads only the private page carrying one selected heap-page container.
pub fn read_offsets<S: PageStore>(
    store: &mut S,
    expected: GroupKey,
    directory: Directory<'_>,
    page: u8,
) -> Result<Option<OffsetMask>> {
    if directory.key() != expected || expected.layout() != store.layout() {
        return Err(Error::InvalidState);
    }
    directory.offsets(page, |extent, output| {
        let image = store.read(extent.block)?;
        if image.kind() != PageKind::Primary {
            return Err(Error::InvalidState);
        }
        output.copy_from_slice(image.primary_extent(extent.offset, extent.len)?);
        Ok(())
    })
}
