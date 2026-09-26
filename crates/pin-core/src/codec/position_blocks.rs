// experimental independent position blocks; not yet a native storage format.
// directory validation is separate from full payload integrity verification.
use super::bytes::{Reader, Writer, var_u32_len};
use super::{Error, ErrorKind, Result};

pub const BLOCK_POSITIONS: usize = 128;
const HEADER: usize = 8;
const ENTRY: usize = 16;
const MAGIC: &[u8; 4] = b"PB01";

/// Checked metadata independent of the positional payload's physical location.
#[derive(Clone, Copy, Debug)]
pub struct PositionDirectory<'a> {
    entries: &'a [u8],
    count: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct PositionBlocks<'a> {
    directory: PositionDirectory<'a>,
    payload: &'a [u8],
}

/// A directory-validated request; offsets are relative to the payload start.
#[derive(Clone, Copy, Debug)]
pub struct PositionBlockRequest {
    first: u32,
    last: u32,
    start: usize,
    end: usize,
    count: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SeekResult {
    pub position: Option<u32>,
    pub decoded_positions: usize,
    pub decoded_bytes: usize,
}

impl PositionBlockRequest {
    pub fn byte_range(self) -> std::ops::Range<usize> {
        self.start..self.end
    }

    // validates the entire selected block before callers may use its result.
    fn decode(self, bytes: &[u8], mut visit: impl FnMut(u32)) -> Result<()> {
        if bytes.len() != self.end - self.start {
            return Err(Error::new(self.start, ErrorKind::InvalidValue));
        }
        let mut reader = Reader::new(bytes);
        let mut position = self.first;
        visit(position);
        for _ in 1..self.count {
            let offset = reader.offset();
            let delta = reader.var_u32()?;
            if delta == 0 {
                return Err(Error::new(offset, ErrorKind::InvalidOrder));
            }
            position = position
                .checked_add(delta)
                .ok_or(Error::new(offset, ErrorKind::Overflow))?;
            visit(position);
        }
        reader.finish()?;
        if position != self.last {
            return Err(Error::new(self.start, ErrorKind::InvalidValue));
        }
        Ok(())
    }

    pub fn seek_ge(self, bytes: &[u8], target: u32) -> Result<SeekResult> {
        let mut found = None;
        self.decode(bytes, |position| {
            if found.is_none() && position >= target {
                found = Some(position);
            }
        })?;
        Ok(SeekResult {
            position: found,
            decoded_positions: self.count,
            decoded_bytes: bytes.len(),
        })
    }

    /// Decodes into caller-owned bounded scratch for repeated nearby seeks.
    /// On error, partially written output must be discarded.
    pub fn decode_into(self, bytes: &[u8], output: &mut [u32]) -> Result<usize> {
        if output.len() < self.count {
            return Err(Error::new(self.start, ErrorKind::Truncated));
        }
        let mut index = 0;
        self.decode(bytes, |position| {
            output[index] = position;
            index += 1;
        })?;
        Ok(index)
    }
}

impl<'a> PositionDirectory<'a> {
    /// Reads the fixed header to bound the following directory read.
    pub fn encoded_len(header: &[u8], max_positions: u32) -> Result<usize> {
        let mut reader = Reader::new(header);
        if reader.take(4)? != MAGIC {
            return Err(Error::new(0, ErrorKind::BadMagic));
        }
        let count = reader.u32()?;
        reader.finish()?;
        if count > max_positions {
            return Err(Error::new(4, ErrorKind::LimitExceeded));
        }
        (count as usize)
            .div_ceil(BLOCK_POSITIONS)
            .checked_mul(ENTRY)
            .and_then(|n| n.checked_add(HEADER))
            .ok_or(Error::new(4, ErrorKind::Overflow))
    }

    /// Checks the whole directory against an external payload's known byte length.
    /// This does not read or certify skipped positional bytes.
    pub fn open(bytes: &'a [u8], payload_bytes: usize, max_positions: u32) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let header = reader.take(HEADER)?;
        let size = Self::encoded_len(header, max_positions)?;
        let count = Reader::new(&header[4..]).u32()?;
        let entries = reader.take(size - HEADER)?;
        reader.finish()?;
        let view = Self { entries, count };
        let mut end = 0;
        let mut previous_last = None;
        for index in 0..view.blocks() {
            let block = view.block(index)?;
            let length = block
                .end
                .checked_sub(block.start)
                .ok_or(Error::new(HEADER + index * ENTRY, ErrorKind::InvalidValue))?;
            if block.start != end
                || block.end > payload_bytes
                || length < block.count - 1
                || length > (block.count - 1) * 5
                || block.first > block.last
                || (block.count == 1 && block.first != block.last)
                || u64::from(block.last) - u64::from(block.first) < (block.count - 1) as u64
            {
                return Err(Error::new(HEADER + index * ENTRY, ErrorKind::InvalidValue));
            }
            if previous_last.is_some_and(|last| last >= block.first) {
                return Err(Error::new(HEADER + index * ENTRY, ErrorKind::InvalidOrder));
            }
            end = block.end;
            previous_last = Some(block.last);
        }
        if end != payload_bytes {
            return Err(Error::new(
                size.saturating_add(end),
                ErrorKind::TrailingBytes,
            ));
        }
        Ok(view)
    }

    pub const fn len(self) -> u32 {
        self.count
    }
    pub const fn is_empty(self) -> bool {
        self.count == 0
    }
    pub fn blocks(self) -> usize {
        self.entries.len() / ENTRY
    }

    fn block(self, index: usize) -> Result<PositionBlockRequest> {
        let mut reader = Reader::new(&self.entries[index * ENTRY..(index + 1) * ENTRY]);
        Ok(PositionBlockRequest {
            first: reader.u32()?,
            last: reader.u32()?,
            start: reader.u32()? as usize,
            end: reader.u32()? as usize,
            count: (self.count as usize - index * BLOCK_POSITIONS).min(BLOCK_POSITIONS),
        })
    }

    /// Fetches at most one selected block into bounded scratch, preserving reader errors.
    /// The callback must fill the exact range from the directory's immutable source.
    pub fn seek_with<E: From<Error>>(
        self,
        target: u32,
        read: impl FnOnce(std::ops::Range<usize>, &mut [u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<SeekResult, E> {
        let Some(block) = self.select(target)? else {
            return Ok(SeekResult::default());
        };
        let range = block.byte_range();
        let mut scratch = [0; (BLOCK_POSITIONS - 1) * 5];
        let bytes = &mut scratch[..range.len()];
        if !bytes.is_empty() {
            read(range, bytes)?;
        }
        Ok(block.seek_ge(bytes, target)?)
    }

    /// Finds at most one block to read, or proves the target exceeds all blocks.
    pub fn select(self, target: u32) -> Result<Option<PositionBlockRequest>> {
        let mut lo = 0;
        let mut hi = self.blocks();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.block(mid)?.last < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == self.blocks() {
            Ok(None)
        } else {
            self.block(lo).map(Some)
        }
    }
}

impl<'a> PositionBlocks<'a> {
    pub fn open(bytes: &'a [u8], max_positions: u32) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let size = PositionDirectory::encoded_len(reader.take(HEADER)?, max_positions)?;
        reader.take(size - HEADER)?;
        let payload = reader.take(reader.remaining())?;
        let directory = PositionDirectory::open(&bytes[..size], payload.len(), max_positions)?;
        Ok(Self { directory, payload })
    }
    pub const fn len(self) -> u32 {
        self.directory.len()
    }
    pub const fn is_empty(self) -> bool {
        self.directory.is_empty()
    }
    pub fn blocks(self) -> usize {
        self.directory.blocks()
    }

    pub fn seek_ge(self, target: u32) -> Result<SeekResult> {
        match self.directory.select(target)? {
            Some(block) => block.seek_ge(&self.payload[block.byte_range()], target),
            None => Ok(SeekResult::default()),
        }
    }
    pub fn iter(self) -> PositionBlockIter<'a> {
        PositionBlockIter {
            view: self,
            next_block: 0,
            remaining: 0,
            reader: Reader::new(&[]),
            previous: 0,
            last: 0,
            failed: false,
        }
    }

    pub fn validate_all(self) -> Result<()> {
        for index in 0..self.blocks() {
            let block = self.directory.block(index)?;
            block.seek_ge(&self.payload[block.byte_range()], 0)?;
        }
        Ok(())
    }
}

pub struct PositionBlockIter<'a> {
    view: PositionBlocks<'a>,
    next_block: usize,
    remaining: usize,
    reader: Reader<'a>,
    previous: u32,
    last: u32,
    failed: bool,
}

impl Iterator for PositionBlockIter<'_> {
    type Item = Result<u32>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || (self.remaining == 0 && self.next_block == self.view.blocks()) {
            return None;
        }
        let result = (|| {
            if self.remaining == 0 {
                let block = self.view.directory.block(self.next_block)?;
                self.next_block += 1;
                self.reader = Reader::new(&self.view.payload[block.byte_range()]);
                self.previous = block.first;
                self.last = block.last;
                self.remaining = block.count - 1;
            } else {
                let delta = self.reader.var_u32()?;
                if delta == 0 {
                    return Err(Error::new(self.reader.offset(), ErrorKind::InvalidOrder));
                }
                self.previous = self
                    .previous
                    .checked_add(delta)
                    .ok_or(Error::new(self.reader.offset(), ErrorKind::Overflow))?;
                self.remaining -= 1;
            }
            if self.remaining == 0 {
                self.reader.finish()?;
                if self.previous != self.last {
                    return Err(Error::new(self.reader.offset(), ErrorKind::InvalidValue));
                }
            }
            Ok(self.previous)
        })();
        self.failed = result.is_err();
        Some(result)
    }
}

impl std::iter::FusedIterator for PositionBlockIter<'_> {}

// validates input and output size before mutation; no allocation or native casts.
pub fn encode(positions: &[u32], output: &mut [u8], max_positions: u32) -> Result<usize> {
    let count =
        u32::try_from(positions.len()).map_err(|_| Error::new(0, ErrorKind::LimitExceeded))?;
    if count > max_positions {
        return Err(Error::new(0, ErrorKind::LimitExceeded));
    }
    for (index, pair) in positions.windows(2).enumerate() {
        if pair[0] >= pair[1] {
            return Err(Error::new(index, ErrorKind::InvalidOrder));
        }
    }
    let blocks = positions.len().div_ceil(BLOCK_POSITIONS);
    let directory_len = blocks
        .checked_mul(ENTRY)
        .and_then(|n| n.checked_add(HEADER))
        .ok_or(Error::new(0, ErrorKind::Overflow))?;
    let mut payload_len = 0usize;
    for block in positions.chunks(BLOCK_POSITIONS) {
        for pair in block.windows(2) {
            payload_len = payload_len
                .checked_add(var_u32_len(pair[1] - pair[0]))
                .ok_or(Error::new(0, ErrorKind::Overflow))?;
        }
    }
    u32::try_from(payload_len).map_err(|_| Error::new(0, ErrorKind::LimitExceeded))?;
    let total = directory_len
        .checked_add(payload_len)
        .ok_or(Error::new(0, ErrorKind::Overflow))?;
    if output.len() < total {
        return Err(Error::new(0, ErrorKind::Truncated));
    }
    let (directory, payload) = output[..total].split_at_mut(directory_len);
    let mut header = Writer::new(directory);
    let mut data = Writer::new(payload);
    header.put(MAGIC)?;
    header.u32(count)?;
    for block in positions.chunks(BLOCK_POSITIONS) {
        header.u32(block[0])?;
        header.u32(block[block.len() - 1])?;
        header.u32(data.len() as u32)?;
        for pair in block.windows(2) {
            data.var_u32(pair[1] - pair[0])?;
        }
        header.u32(data.len() as u32)?;
    }
    Ok(total)
}
