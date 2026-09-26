//! versioned physical extent map plus an inline prefix of one PD02 document.
use super::*;

pub const DIRECT_PREFIX_BYTES: usize = 3072;
pub const MAX_DIRECT_FRAGMENTS: usize =
    (MAX_DOCUMENT_BYTES - DIRECT_PREFIX_BYTES).div_ceil(FRAGMENT_BYTES);
const DIRECTORY_HEADER: usize = 48;
const _: () =
    assert!(DIRECTORY_HEADER + MAX_DIRECT_FRAGMENTS * 4 + DIRECT_PREFIX_BYTES <= CAPACITY);

/// Returns the prefix and tail count that fill the directory page.
pub fn direct_document_layout(total: usize) -> Result<(usize, usize)> {
    if !(INLINE_BYTES + 1..=MAX_DOCUMENT_BYTES).contains(&total) {
        return Err(corrupt(DIRECTORY_HEADER));
    }
    let available = CAPACITY - DIRECTORY_HEADER;
    let count = total.saturating_sub(available).div_ceil(FRAGMENT_BYTES - 4);
    let prefix = total.min(available - count * 4);
    Ok((prefix, count))
}

#[derive(Clone, Copy)]
pub struct DocumentDirectory<'a> {
    pub owner: OwnerRef,
    pub total: usize,
    pub prefix: &'a [u8],
    entries: &'a [u8],
}

impl DocumentDirectory<'_> {
    pub fn fragments(&self) -> usize {
        self.entries.len() / 4
    }
    pub fn block(&self, index: usize) -> Result<u32> {
        let start = index.checked_mul(4).ok_or(Error::InvalidState)?;
        let end = start.checked_add(4).ok_or(Error::InvalidState)?;
        Ok(Reader::new(self.entries.get(start..end).ok_or(Error::InvalidState)?).u32()?)
    }
}

impl Page {
    pub fn direct_documents(&self) -> Result<bool> {
        self.require(PageKind::Meta)?;
        let flags = self.u32(28)?;
        if flags & !1 != 0 {
            return Err(corrupt(28));
        }
        Ok(flags & 1 != 0)
    }

    pub fn enable_direct_documents(&mut self) -> Result<()> {
        self.require(PageKind::Meta)?;
        self.put_u32(28, 1)
    }

    pub fn document_directory(
        block: u32,
        owner: OwnerRef,
        total: usize,
        blocks: &[u32],
        prefix: &[u8],
    ) -> Result<Self> {
        let (prefix_len, count) = direct_document_layout(total)?;
        if prefix.len() != prefix_len
            || blocks.len() != count
            || blocks.len() > MAX_DIRECT_FRAGMENTS
            || blocks
                .iter()
                .any(|&value| !block_valid(value) || value == block)
        {
            return Err(corrupt(DIRECTORY_HEADER));
        }
        let mut page = Self::new(block, PageKind::DocumentDirectory)?;
        page.len = DIRECTORY_HEADER + blocks.len() * 4 + prefix.len();
        if page.len > CAPACITY {
            return Err(corrupt(DIRECTORY_HEADER));
        }
        let mut writer = Writer::new(&mut page.bytes[HEADER..page.len]);
        owner.write(&mut writer)?;
        writer.u32(total as u32)?;
        writer.u32(blocks.len() as u32)?;
        writer.u32(prefix.len() as u32)?;
        writer.u32(1)?;
        for &value in blocks {
            writer.u32(value)?;
        }
        writer.put(prefix)?;
        page.set_next(blocks.first().copied().unwrap_or(NO_BLOCK))?;
        Ok(page)
    }

    pub fn document_directory_data(&self) -> Result<DocumentDirectory<'_>> {
        self.require(PageKind::DocumentDirectory)?;
        let mut reader = Reader::new(&self.bytes()[HEADER..]);
        let owner = OwnerRef::read(&mut reader)?;
        let total = reader.u32()? as usize;
        let count = reader.u32()? as usize;
        let prefix_len = reader.u32()? as usize;
        let version = reader.u32()?;
        let expected = match version {
            0 if (INLINE_BYTES + 1..=MAX_DOCUMENT_BYTES).contains(&total) => (
                DIRECT_PREFIX_BYTES,
                (total - DIRECT_PREFIX_BYTES).div_ceil(FRAGMENT_BYTES),
            ),
            1 => direct_document_layout(total)?,
            _ => return Err(corrupt(44)),
        };
        if (prefix_len, count) != expected || count > MAX_DIRECT_FRAGMENTS {
            return Err(corrupt(DIRECTORY_HEADER));
        }
        let entries = reader.take(count * 4)?;
        let prefix = reader.take(prefix_len)?;
        reader.finish()?;
        let directory = DocumentDirectory {
            owner,
            total,
            prefix,
            entries,
        };
        for index in 0..count {
            let block = directory.block(index)?;
            if !block_valid(block) || block == self.block {
                return Err(corrupt(DIRECTORY_HEADER + index * 4));
            }
        }
        if self.next()?
            != if count == 0 {
                NO_BLOCK
            } else {
                directory.block(0)?
            }
        {
            return Err(corrupt(12));
        }
        Ok(directory)
    }
}
