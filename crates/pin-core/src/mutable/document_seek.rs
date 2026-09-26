//! selected PD02 streams fetched through a checked physical extent map.
use super::document::{MAX_DOCUMENT_BYTES, MAX_TERM_BYTES};
use super::page::{DocumentDirectory, FRAGMENT_BYTES, NO_BLOCK, Owner, Page, PageKind};
use super::{PageStore, load, load_into};
use crate::analysis::PROFILE_ID;
use crate::codec::bytes::Reader;
use crate::error::{Error, Result};

struct Input<'a, S> {
    store: &'a mut S,
    directory: DocumentDirectory<'a>,
    cache: &'a mut Option<Page>,
}

impl<S: PageStore> Input<'_, S> {
    fn read(&mut self, mut offset: usize, mut output: &mut [u8]) -> Result<()> {
        if offset
            .checked_add(output.len())
            .is_none_or(|end| end > self.directory.total)
        {
            return Err(Error::InvalidDocument);
        }
        while !output.is_empty() {
            let (start, source) = if offset < self.directory.prefix.len() {
                (0, self.directory.prefix)
            } else {
                let index = (offset - self.directory.prefix.len()) / FRAGMENT_BYTES;
                let start = self.directory.prefix.len() + index * FRAGMENT_BYTES;
                let block = self.directory.block(index)?;
                if self.cache.as_ref().is_none_or(|page| page.block() != block) {
                    if let Some(page) = self.cache.as_mut() {
                        load_into(self.store, block, PageKind::Fragment, page)?;
                    } else {
                        *self.cache = Some(load(self.store, block, PageKind::Fragment)?);
                    }
                }
                let page = self.cache.as_ref().ok_or(Error::InvalidState)?;
                let (owner, current, payload) = page.fragment_data()?;
                let next = if index + 1 < self.directory.fragments() {
                    self.directory.block(index + 1)?
                } else {
                    NO_BLOCK
                };
                if owner != self.directory.owner
                    || current as usize != start
                    || payload.len() != FRAGMENT_BYTES.min(self.directory.total - start)
                    || page.next()? != next
                {
                    return Err(Error::InvalidState);
                }
                (start, payload)
            };
            let within = offset - start;
            let length = output.len().min(source.len() - within);
            output[..length].copy_from_slice(&source[within..within + length]);
            offset += length;
            output = &mut output[length..];
        }
        Ok(())
    }
}

pub(super) fn copy_all<S: PageStore>(
    store: &mut S,
    head: &Page,
    owner: Owner<'_>,
    output: &mut [u8],
) -> Result<()> {
    let directory = head.document_directory_data()?;
    if directory.owner != owner.reference
        || directory.total != owner.data_bytes as usize
        || output.len() != directory.total
    {
        return Err(Error::InvalidDocument);
    }
    Input {
        store,
        directory,
        cache: &mut None,
    }
    .read(0, output)
}

#[derive(Clone, Copy, Default)]
struct Range {
    start: usize,
    length: usize,
    buffer: usize,
}

pub(super) fn matches<S: PageStore>(
    store: &mut S,
    head: &Page,
    owner: Owner<'_>,
    wanted: &[String],
    memory_bytes: usize,
    bytes: &mut Vec<u8>,
    cache: &mut Option<Page>,
) -> Result<Option<bool>> {
    let directory = head.document_directory_data()?;
    if directory.owner != owner.reference
        || directory.total != owner.data_bytes as usize
        || wanted.is_empty()
        || wanted.len() > 64
        || directory.total > MAX_DOCUMENT_BYTES
    {
        return Err(Error::InvalidDocument);
    }
    if directory.prefix.starts_with(b"PD03") {
        let page_cost = if cache.is_none() {
            std::mem::size_of::<Page>()
        } else {
            0
        };
        let Some(budget) = memory_bytes.checked_sub(page_cost) else {
            return Ok(None);
        };
        let mut input = Input {
            store,
            directory,
            cache,
        };
        return super::block_phrase::matches(
            directory.total,
            owner.tokens,
            owner.terms,
            wanted,
            budget,
            bytes,
            |offset, output| input.read(offset, output),
        );
    }
    let fixed = if cache.is_none() {
        std::mem::size_of::<Page>()
    } else {
        0
    } + std::mem::size_of::<[Range; 64]>()
        + std::mem::size_of::<[usize; 64]>()
        + MAX_TERM_BYTES * 2;
    let Some(budget) = memory_bytes.checked_sub(fixed) else {
        return Ok(None);
    };
    if bytes.capacity() > budget {
        return Ok(None);
    }
    let mut input = Input {
        store,
        directory,
        cache,
    };
    let mut header = [0; 16];
    input.read(0, &mut header)?;
    let mut reader = Reader::new(&header);
    if reader.take(4)? != b"PD02"
        || reader.u32()? != PROFILE_ID
        || reader.u32()? != owner.tokens
        || reader.u32()? != owner.terms
    {
        return Err(Error::InvalidDocument);
    }
    let mut ranges = [Range::default(); 64];
    let mut mapping = [usize::MAX; 64];
    let mut found = 0;
    let mut selected = 0;
    let mut offset = 16usize;
    let mut previous = [0; MAX_TERM_BYTES];
    let mut previous_len = 0;
    let mut frequency = 0u32;
    let mut buffer_size = 0usize;
    for _ in 0..owner.terms {
        let mut entry = [0; 8];
        input.read(offset, &mut entry)?;
        let mut reader = Reader::new(&entry);
        let term_len = reader.u16()? as usize;
        if term_len == 0 || term_len > MAX_TERM_BYTES || reader.u16()? != 0 {
            return Err(Error::InvalidDocument);
        }
        let size = reader.u32()? as usize;
        if size < 4 {
            return Err(Error::InvalidDocument);
        }
        let mut term = [0; MAX_TERM_BYTES];
        input.read(offset + 8, &mut term[..term_len])?;
        let name = std::str::from_utf8(&term[..term_len]).map_err(|_| Error::InvalidDocument)?;
        if term[..term_len] <= previous[..previous_len] {
            return Err(Error::InvalidDocument);
        }
        previous[..term_len].copy_from_slice(&term[..term_len]);
        previous_len = term_len;
        let start = offset
            .checked_add(8 + term_len)
            .ok_or(Error::InvalidDocument)?;
        let end = start.checked_add(size).ok_or(Error::InvalidDocument)?;
        if end > directory.total {
            return Err(Error::InvalidDocument);
        }
        let mut count = [0; 4];
        input.read(start, &mut count)?;
        let count = u32::from_le_bytes(count);
        if count == 0 || count > owner.tokens || count as usize > size - 4 {
            return Err(Error::InvalidDocument);
        }
        frequency = frequency.checked_add(count).ok_or(Error::InvalidDocument)?;
        if frequency > owner.tokens {
            return Err(Error::InvalidDocument);
        }
        let mut used = false;
        for (index, wanted) in wanted.iter().enumerate() {
            if wanted == name {
                mapping[index] = selected;
                found += 1;
                used = true;
            }
        }
        if used {
            ranges[selected] = Range {
                start,
                length: size,
                buffer: buffer_size,
            };
            buffer_size = buffer_size
                .checked_add(size)
                .ok_or(Error::InvalidDocument)?;
            selected += 1;
        }
        offset = end;
        if found == wanted.len() {
            break;
        }
    }
    if found != wanted.len() {
        if offset != directory.total || frequency != owner.tokens {
            return Err(Error::InvalidDocument);
        }
        return Ok(Some(false));
    }
    if buffer_size > budget {
        return Ok(None);
    }
    bytes.clear();
    if bytes.try_reserve_exact(buffer_size).is_err() || bytes.capacity() > budget {
        *bytes = Vec::new();
        return Ok(None);
    }
    bytes.resize(buffer_size, 0);
    for range in &ranges[..selected] {
        input.read(
            range.start,
            &mut bytes[range.buffer..range.buffer + range.length],
        )?;
    }
    let mut streams: [&[u8]; 64] = [&[]; 64];
    for (index, &slot) in mapping[..wanted.len()].iter().enumerate() {
        let range = ranges[slot];
        streams[index] = &bytes[range.buffer..range.buffer + range.length];
    }
    super::phrase_prefix::matches_positions(&streams[..wanted.len()], owner.tokens).map(Some)
}
