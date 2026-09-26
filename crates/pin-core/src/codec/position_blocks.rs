// experimental independent position blocks; not yet a native storage format.
// directory validation is separate from full payload integrity verification.
use super::bytes::{Reader, Writer, var_u32_len};
use super::{Error, ErrorKind, Result};

pub const BLOCK_POSITIONS: usize = 128;
const HEADER: usize = 8;
const ENTRY: usize = 16;
const MAGIC: &[u8; 4] = b"PB01";

#[derive(Clone, Copy, Debug)]
pub struct PositionBlocks<'a> {
    directory: &'a [u8],
    payload: &'a [u8],
    count: u32,
}

#[derive(Clone, Copy)]
struct Block {
    first: u32,
    last: u32,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SeekResult {
    pub position: Option<u32>,
    pub decoded_positions: usize,
    pub decoded_bytes: usize,
}

impl<'a> PositionBlocks<'a> {
    // checks all directory extents and bounds, without reading delta payloads.
    pub fn open(bytes: &'a [u8], max_positions: u32) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != MAGIC {
            return Err(Error::new(0, ErrorKind::BadMagic));
        }
        let count = reader.u32()?;
        if count > max_positions {
            return Err(Error::new(4, ErrorKind::LimitExceeded));
        }
        let blocks = (count as usize).div_ceil(BLOCK_POSITIONS);
        let directory_len = blocks
            .checked_mul(ENTRY)
            .ok_or(Error::new(4, ErrorKind::Overflow))?;
        let directory = reader.take(directory_len)?;
        let payload = reader.take(reader.remaining())?;
        let view = Self {
            directory,
            payload,
            count,
        };
        let mut end = 0;
        let mut previous_last = None;
        for index in 0..blocks {
            let block = view.block(index)?;
            let n = view.block_len(index);
            let length = block
                .end
                .checked_sub(block.start)
                .ok_or(Error::new(HEADER + index * ENTRY, ErrorKind::InvalidValue))?;
            if block.start != end
                || block.end > payload.len()
                || length < n - 1
                || length > (n - 1) * 5
                || block.first > block.last
                || (n == 1 && block.first != block.last)
                || u64::from(block.last) - u64::from(block.first) < (n - 1) as u64
            {
                return Err(Error::new(HEADER + index * ENTRY, ErrorKind::InvalidValue));
            }
            if previous_last.is_some_and(|last| last >= block.first) {
                return Err(Error::new(HEADER + index * ENTRY, ErrorKind::InvalidOrder));
            }
            end = block.end;
            previous_last = Some(block.last);
        }
        if end != payload.len() {
            return Err(Error::new(
                HEADER + directory_len + end,
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
        self.directory.len() / ENTRY
    }

    fn block_len(self, index: usize) -> usize {
        (self.count as usize - index * BLOCK_POSITIONS).min(BLOCK_POSITIONS)
    }

    fn block(self, index: usize) -> Result<Block> {
        let mut reader = Reader::new(&self.directory[index * ENTRY..(index + 1) * ENTRY]);
        Ok(Block {
            first: reader.u32()?,
            last: reader.u32()?,
            start: reader.u32()? as usize,
            end: reader.u32()? as usize,
        })
    }

    // validates the whole selected block, even when the answer appears early.
    fn decode(self, index: usize, target: u32) -> Result<SeekResult> {
        let block = self.block(index)?;
        let mut reader = Reader::new(&self.payload[block.start..block.end]);
        let mut position = block.first;
        let mut found = (position >= target).then_some(position);
        for _ in 1..self.block_len(index) {
            let offset = reader.offset();
            let delta = reader.var_u32()?;
            if delta == 0 {
                return Err(Error::new(offset, ErrorKind::InvalidOrder));
            }
            position = position
                .checked_add(delta)
                .ok_or(Error::new(offset, ErrorKind::Overflow))?;
            if found.is_none() && position >= target {
                found = Some(position);
            }
        }
        reader.finish()?;
        if position != block.last {
            return Err(Error::new(block.start, ErrorKind::InvalidValue));
        }
        Ok(SeekResult {
            position: found,
            decoded_positions: self.block_len(index),
            decoded_bytes: block.end - block.start,
        })
    }

    // binary search uses checked directory bounds; skipped payloads are not certified.
    pub fn seek_ge(self, target: u32) -> Result<SeekResult> {
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
            return Ok(SeekResult::default());
        }
        self.decode(lo, target)
    }

    pub fn validate_all(self) -> Result<()> {
        for index in 0..self.blocks() {
            self.decode(index, 0)?;
        }
        Ok(())
    }
}

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
