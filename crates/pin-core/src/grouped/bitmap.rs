//! Borrowed bitmap records with independently addressable offset payloads.
//! Opening validates the directory, not offset contents in unvisited pages.

use super::{GROUP_PAGES, GroupKey, HEADER_BYTES, Header, contains, page_count};
use crate::codec::bytes::Writer;
use crate::error::{Error, Result};
use pin_kernels::grouped::{OffsetMask, PageMask, Pages};

const DIRECTORY_BYTES: usize = 4;
pub const MAX_BITMAP_BYTES: usize = HEADER_BYTES + GROUP_PAGES * (DIRECTORY_BYTES + 64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BitmapKind {
    Posting = 1,
    Liveness = 2,
}

/// One present heap page; bit zero represents heap offset one.
#[derive(Clone, Debug)]
pub struct PageOffsets {
    pub page: u8,
    pub offsets: OffsetMask,
}

/// A checked directory with lazily validated offset payloads.
/// Liveness retains its original directory and widths after bits are cleared.
#[derive(Clone, Copy)]
pub struct Bitmap<'a> {
    bytes: &'a [u8],
    header: Header,
    body: usize,
    prefixes: [u16; 4],
}

impl<'a> Bitmap<'a> {
    /// Opens a bounded record without allocation or decoding any offset payload.
    ///
    /// # Errors
    /// Rejects unknown versions, invalid identities, nonpacked directories,
    /// reserved fields, invalid widths, truncated input and trailing bytes.
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() > MAX_BITMAP_BYTES {
            return Err(Error::Limit("bitmap group bytes"));
        }
        let header = Header::read(bytes)?;
        if !matches!(header.kind, 1 | 2) || header.count != 0 {
            return Err(Error::InvalidState);
        }
        let pages = usize::from(page_count(&header.mask));
        let body = HEADER_BYTES + pages * DIRECTORY_BYTES;
        if body > bytes.len() {
            return Err(Error::InvalidState);
        }
        let mut end = 0;
        for entry in bytes[HEADER_BYTES..body].as_chunks::<DIRECTORY_BYTES>().0 {
            let start = usize::from(u16::from_le_bytes([entry[0], entry[1]]));
            let len = usize::from(entry[2]);
            if start != end
                || len == 0
                || len > usize::from(header.key.layout.max_offset()).div_ceil(8)
                || entry[3] != 0
            {
                return Err(Error::InvalidState);
            }
            end += len;
        }
        if end != bytes.len() - body {
            return Err(Error::InvalidState);
        }
        let mut prefixes = [0; 4];
        for index in 1..4 {
            prefixes[index] = prefixes[index - 1] + header.mask[index - 1].count_ones() as u16;
        }
        Ok(Self {
            bytes,
            header,
            body,
            prefixes,
        })
    }

    pub const fn key(&self) -> GroupKey {
        self.header.key
    }

    pub fn kind(&self) -> BitmapKind {
        if self.header.kind == 1 {
            BitmapKind::Posting
        } else {
            BitmapKind::Liveness
        }
    }

    pub const fn pages(&self) -> &PageMask {
        &self.header.mask
    }

    /// Decodes only one selected page into initialized, fixed-size scratch.
    ///
    /// # Errors
    /// Rejects out-of-domain tail bits and noncanonical posting payloads.
    /// An absent page is empty; a retired liveness page may also be empty.
    pub fn offsets(&self, page: u8) -> Result<OffsetMask> {
        let mut result = [0; 8];
        let Some((start, len)) = self.span(page) else {
            return Ok(result);
        };
        let bytes = &self.bytes[start..start + len];
        let last = bytes[len - 1];
        let domain = usize::from(self.key().layout.max_offset());
        if (self.kind() == BitmapKind::Posting && last == 0)
            || (len * 8 > domain && last >> (domain % 8) != 0)
        {
            return Err(Error::InvalidState);
        }
        let (words, tail) = bytes.as_chunks::<8>();
        for (output, word) in result.iter_mut().zip(words) {
            *output = u64::from_le_bytes(*word);
        }
        if !tail.is_empty() {
            let mut word = [0; 8];
            word[..tail.len()].copy_from_slice(tail);
            result[words.len()] = u64::from_le_bytes(word);
        }
        Ok(result)
    }

    pub fn payload_bytes(&self, page: u8) -> usize {
        self.span(page).map_or(0, |(_, len)| len)
    }

    /// Checks every payload for build verification and maintenance, not pruning.
    ///
    /// # Errors
    /// Returns the first offset payload error.
    pub fn validate_all(&self) -> Result<()> {
        for page in Pages::new(self.header.mask) {
            self.offsets(page)?;
        }
        Ok(())
    }

    pub(super) fn span(&self, page: u8) -> Option<(usize, usize)> {
        if !contains(&self.header.mask, page) {
            return None;
        }
        let word = usize::from(page) / 64;
        let lower = (1u64 << (page % 64)) - 1;
        let rank = usize::from(self.prefixes[word])
            + (self.header.mask[word] & lower).count_ones() as usize;
        let entry = HEADER_BYTES + rank * DIRECTORY_BYTES;
        let start = usize::from(u16::from_le_bytes([
            self.bytes[entry],
            self.bytes[entry + 1],
        ]));
        Some((self.body + start, usize::from(self.bytes[entry + 2])))
    }
}

/// Encodes sorted nonempty pages into caller-owned storage without allocation.
/// Invalid input and insufficient capacity are rejected before any write.
///
/// # Errors
/// Rejects duplicates, empty page masks, invalid tail bits and capacity limits.
pub fn encode_bitmap(
    key: GroupKey,
    kind: BitmapKind,
    pages: &[PageOffsets],
    output: &mut [u8],
) -> Result<usize> {
    let mut mask = [0; 4];
    let mut previous = None;
    for page in pages {
        if previous.is_some_and(|previous| previous >= page.page) {
            return Err(Error::InvalidState);
        }
        key.block(page.page)?;
        bitmap_len(key, &page.offsets)?;
        super::insert(&mut mask, page.page);
        previous = Some(page.page);
    }
    encode_with(key, kind, mask, output, |page| {
        let index = pages
            .binary_search_by_key(&page, |entry| entry.page)
            .map_err(|_| Error::InvalidState)?;
        Ok(pages[index].offsets)
    })
}

pub(super) fn encode_with(
    key: GroupKey,
    kind: BitmapKind,
    mask: PageMask,
    output: &mut [u8],
    mut offsets: impl FnMut(u8) -> Result<OffsetMask>,
) -> Result<usize> {
    let body = HEADER_BYTES + usize::from(page_count(&mask)) * DIRECTORY_BYTES;
    let mut length = body;
    for page in Pages::new(mask) {
        key.block(page)?;
        length += bitmap_len(key, &offsets(page)?)?;
    }
    if length > output.len() {
        return Err(Error::Limit("bitmap group output"));
    }
    let mut writer = Writer::new(&mut output[..length]);
    Header {
        key,
        kind: kind as u8,
        count: 0,
        mask,
    }
    .write(&mut writer, length)?;
    let mut start = 0;
    for page in Pages::new(mask) {
        let len = bitmap_len(key, &offsets(page)?)?;
        writer.u16(start as u16)?;
        writer.u8(len as u8)?;
        writer.u8(0)?;
        start += len;
    }
    for page in Pages::new(mask) {
        let words = offsets(page)?;
        let len = bitmap_len(key, &words)?;
        for index in 0..len {
            writer.u8((words[index / 8] >> ((index % 8) * 8)) as u8)?;
        }
    }
    Ok(length)
}

pub(super) fn bitmap_len(key: GroupKey, offsets: &OffsetMask) -> Result<usize> {
    let Some(last) = offsets.iter().rposition(|&word| word != 0) else {
        return Err(Error::InvalidState);
    };
    let bits = last * 64 + (64 - offsets[last].leading_zeros() as usize);
    if bits > usize::from(key.layout.max_offset()) {
        return Err(Error::InvalidState);
    }
    Ok(bits.div_ceil(8))
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use crate::identity::{Generation, HeapLayout, SegmentId};

    #[test]
    fn word_decoding_covers_every_width_and_unaligned_record() {
        for maximum in [291, 512] {
            let key = GroupKey::new(
                Generation::new(1).unwrap(),
                SegmentId::new(1).unwrap(),
                0,
                HeapLayout::new(maximum).unwrap(),
            )
            .unwrap();
            for offset in 1..=maximum {
                let mut expected = [0; 8];
                // fill every bit below the last offset, including every word boundary.
                for bit in 0..usize::from(offset) {
                    expected[bit / 64] |= 1 << (bit % 64);
                }
                for kind in [BitmapKind::Posting, BitmapKind::Liveness] {
                    let mut bytes = [0; MAX_BITMAP_BYTES + 1];
                    let len = encode_bitmap(
                        key,
                        kind,
                        &[PageOffsets { page: 255, offsets: expected }],
                        &mut bytes[1..],
                    )
                    .unwrap();
                    let bitmap = Bitmap::open(&bytes[1..1 + len]).unwrap();
                    assert_eq!(bitmap.offsets(255).unwrap(), expected);
                    assert_eq!(bitmap.offsets(0).unwrap(), [0; 8]);
                }
            }
        }
    }
}
