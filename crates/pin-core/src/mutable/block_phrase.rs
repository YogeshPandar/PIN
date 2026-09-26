//! selected PD03 phrase witnesses over a bounded external byte reader.
use super::document::{MAX_DOCUMENT_BYTES, MAX_DOCUMENT_TOKENS, MAX_TERM_BYTES};
use crate::analysis::PROFILE_ID;
use crate::codec::bytes::Reader;
use crate::codec::position_blocks::PositionDirectory;
use crate::error::{Error, Result};

#[derive(Clone, Copy, Default)]
struct Descriptor {
    start: usize,
    size: usize,
    retained: usize,
    length: usize,
    count: u32,
    blocked: bool,
}
#[derive(Clone, Copy)]
enum View<'a> {
    Deltas(&'a [u8]),
    Blocks(PositionDirectory<'a>, usize),
}
#[derive(Clone, Copy)]
struct Cursor {
    decoded: [u32; 128],
    length: usize,
    delta_offset: usize,
    consumed: u32,
    current: Option<u32>,
}
impl Default for Cursor {
    fn default() -> Self {
        Self {
            decoded: [0; 128],
            length: 0,
            delta_offset: 4,
            consumed: 0,
            current: None,
        }
    }
}
impl Cursor {
    fn seek(
        &mut self,
        view: View<'_>,
        target: u32,
        count: u32,
        tokens: u32,
        read: &mut impl FnMut(usize, &mut [u8]) -> Result<()>,
    ) -> Result<Option<u32>> {
        if self.current.is_some_and(|position| position >= target) {
            return Ok(self.current);
        }
        match view {
            View::Deltas(bytes) => {
                while self.consumed < count {
                    let mut reader = Reader::new(&bytes[self.delta_offset..]);
                    let delta = reader.var_u32()?;
                    if self.consumed != 0 && delta == 0 {
                        return Err(Error::InvalidDocument);
                    }
                    let position = self
                        .current
                        .unwrap_or(0)
                        .checked_add(delta)
                        .ok_or(Error::InvalidDocument)?;
                    if position >= tokens {
                        return Err(Error::InvalidDocument);
                    }
                    self.delta_offset += reader.offset();
                    self.consumed += 1;
                    self.current = Some(position);
                    if self.consumed == count && self.delta_offset != bytes.len() {
                        return Err(Error::InvalidDocument);
                    }
                    if position >= target {
                        return Ok(Some(position));
                    }
                }
                Ok(None)
            }
            View::Blocks(directory, payload) => {
                if self.length == 0 || self.decoded[self.length - 1] < target {
                    let Some(block) = directory.select(target)? else {
                        return Ok(None);
                    };
                    let range = block.byte_range();
                    let mut scratch = [0; 635];
                    if !range.is_empty() {
                        read(payload + range.start, &mut scratch[..range.len()])?;
                    }
                    self.length = block.decode_into(&scratch[..range.len()], &mut self.decoded)?;
                    if self.decoded[self.length - 1] >= tokens {
                        return Err(Error::InvalidDocument);
                    }
                }
                let index =
                    self.decoded[..self.length].partition_point(|&position| position < target);
                self.current = self
                    .decoded
                    .get(index)
                    .copied()
                    .filter(|_| index < self.length);
                Ok(self.current)
            }
        }
    }
}

/// None means bounded scratch is insufficient; no partial query result is emitted.
/// Read offsets refer to one immutable PD03 document and must be filled exactly.
pub fn matches(
    total: usize,
    tokens: u32,
    terms: u32,
    wanted: &[String],
    memory_bytes: usize,
    retained: &mut Vec<u8>,
    mut source: impl FnMut(usize, &mut [u8]) -> Result<()>,
) -> Result<Option<bool>> {
    let mut read = |offset: usize, output: &mut [u8]| {
        if offset
            .checked_add(output.len())
            .is_none_or(|end| end > total)
        {
            return Err(Error::InvalidDocument);
        }
        source(offset, output)
    };
    if total > MAX_DOCUMENT_BYTES
        || tokens > MAX_DOCUMENT_TOKENS
        || terms > tokens
        || wanted.is_empty()
        || wanted.len() > 64
    {
        return Err(Error::InvalidDocument);
    }
    let fixed = std::mem::size_of::<[Descriptor; 64]>()
        + std::mem::size_of::<[Cursor; 64]>()
        + std::mem::size_of::<[Option<View<'_>>; 64]>()
        + std::mem::size_of::<[usize; 64]>()
        + 2 * MAX_TERM_BYTES
        + 635
        + 32;
    let Some(budget) = memory_bytes.checked_sub(fixed) else {
        return Ok(None);
    };
    if retained.capacity() > budget {
        return Ok(None);
    }
    let mut header = [0; 16];
    read(0, &mut header)?;
    let mut input = Reader::new(&header);
    if input.take(4)? != b"PD03"
        || input.u32()? != PROFILE_ID
        || input.u32()? != tokens
        || input.u32()? != terms
    {
        return Err(Error::InvalidDocument);
    }
    let mut descriptors = [Descriptor::default(); 64];
    let mut mapping = [usize::MAX; 64];
    let (mut selected, mut found, mut offset, mut frequency) = (0, 0, 16usize, 0u32);
    let mut previous = [0; MAX_TERM_BYTES];
    let mut previous_len = 0;
    retained.clear();
    for _ in 0..terms {
        let mut entry = [0; 8];
        read(offset, &mut entry)?;
        let mut input = Reader::new(&entry);
        let term_len = input.u16()? as usize;
        let encoding = input.u16()?;
        let size = input.u32()? as usize;
        if term_len == 0 || term_len > MAX_TERM_BYTES || encoding > 1 {
            return Err(Error::InvalidDocument);
        }
        let mut name = [0; MAX_TERM_BYTES];
        read(offset + 8, &mut name[..term_len])?;
        let term = std::str::from_utf8(&name[..term_len]).map_err(|_| Error::InvalidDocument)?;
        if name[..term_len] <= previous[..previous_len] {
            return Err(Error::InvalidDocument);
        }
        previous[..term_len].copy_from_slice(&name[..term_len]);
        previous_len = term_len;
        let start = offset
            .checked_add(8 + term_len)
            .ok_or(Error::InvalidDocument)?;
        let end = start.checked_add(size).ok_or(Error::InvalidDocument)?;
        if end > total || size < if encoding == 1 { 8 } else { 4 } {
            return Err(Error::InvalidDocument);
        }
        let mut prefix = [0; 8];
        let prefix_len = if encoding == 1 { 8 } else { 4 };
        read(start, &mut prefix[..prefix_len])?;
        let (count, length) = if encoding == 1 {
            let length = PositionDirectory::encoded_len(&prefix, tokens)?;
            (
                u32::from_le_bytes(
                    prefix[4..8]
                        .try_into()
                        .map_err(|_| Error::InvalidDocument)?,
                ),
                length,
            )
        } else {
            (
                u32::from_le_bytes(prefix[..4].try_into().map_err(|_| Error::InvalidDocument)?),
                size,
            )
        };
        if count == 0
            || count > tokens
            || length > size
            || (encoding == 0 && count as usize > size - 4)
        {
            return Err(Error::InvalidDocument);
        }
        frequency = frequency.checked_add(count).ok_or(Error::InvalidDocument)?;
        if frequency > tokens {
            return Err(Error::InvalidDocument);
        }
        let mut used = false;
        for (index, word) in wanted.iter().enumerate() {
            if word == term {
                mapping[index] = selected;
                found += 1;
                used = true;
            }
        }
        if used {
            let at = retained.len();
            let required = at.checked_add(length).ok_or(Error::InvalidDocument)?;
            if required > budget {
                return Ok(None);
            }
            if retained.try_reserve_exact(length).is_err() || retained.capacity() > budget {
                *retained = Vec::new();
                return Ok(None);
            }
            retained.resize(required, 0);
            read(start, &mut retained[at..required])?;
            descriptors[selected] = Descriptor {
                start,
                size,
                retained: at,
                length,
                count,
                blocked: encoding == 1,
            };
            selected += 1;
        }
        offset = end;
        if found == wanted.len() {
            break;
        }
    }
    if found != wanted.len() {
        if offset != total || frequency != tokens {
            return Err(Error::InvalidDocument);
        }
        return Ok(Some(false));
    }
    let mut views = [None; 64];
    for (index, descriptor) in descriptors[..selected].iter().enumerate() {
        let bytes = &retained[descriptor.retained..descriptor.retained + descriptor.length];
        views[index] = Some(if descriptor.blocked {
            View::Blocks(
                PositionDirectory::open(bytes, descriptor.size - descriptor.length, tokens)?,
                descriptor.start + descriptor.length,
            )
        } else {
            View::Deltas(bytes)
        });
    }
    let mut cursors = [Cursor::default(); 64];
    let anchor = (0..wanted.len())
        .min_by_key(|&i| descriptors[mapping[i]].count)
        .ok_or(Error::InvalidState)?;
    let mut target = 0;
    loop {
        let descriptor = descriptors[mapping[anchor]];
        let Some(position) = cursors[anchor].seek(
            views[mapping[anchor]].ok_or(Error::InvalidState)?,
            target,
            descriptor.count,
            tokens,
            &mut read,
        )?
        else {
            return Ok(Some(false));
        };
        target = position.checked_add(1).ok_or(Error::InvalidDocument)?;
        let Some(start) = position.checked_sub(anchor as u32) else {
            continue;
        };
        let mut matched = true;
        for index in 0..wanted.len() {
            if index == anchor {
                continue;
            }
            let target = start
                .checked_add(index as u32)
                .ok_or(Error::InvalidDocument)?;
            let descriptor = descriptors[mapping[index]];
            let value = cursors[index].seek(
                views[mapping[index]].ok_or(Error::InvalidState)?,
                target,
                descriptor.count,
                tokens,
                &mut read,
            )?;
            if value.is_none() {
                return Ok(Some(false));
            }
            if value != Some(target) {
                matched = false;
                break;
            }
        }
        if matched {
            return Ok(Some(true));
        }
    }
}
