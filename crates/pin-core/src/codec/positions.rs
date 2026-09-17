// counted delta streams with exact positions and allocation-free iteration.
// construction checks every delta; positions remain scoped to one document term.

use super::bytes::{Reader, Writer, var_u32_len};
use super::{Error, ErrorKind, Result};

#[derive(Clone, Copy, Debug)]
pub struct Positions<'a> {
    payload: &'a [u8],
    count: u32,
}

impl<'a> Positions<'a> {
    // validates count, canonical deltas, strict order and complete consumption.
    pub fn parse(bytes: &'a [u8], max_positions: u32) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let count = reader.u32()?;
        if count > max_positions {
            return Err(Error::new(0, ErrorKind::LimitExceeded));
        }
        if u64::from(count) > reader.remaining() as u64 {
            return Err(Error::new(4, ErrorKind::Truncated));
        }
        let payload = reader.take(reader.remaining())?;
        let view = Self { payload, count };
        let mut iter = view.iter();
        for position in iter.by_ref() {
            position?;
        }
        iter.reader.finish()?;
        Ok(view)
    }

    pub const fn len(self) -> u32 {
        self.count
    }

    pub const fn is_empty(self) -> bool {
        self.count == 0
    }

    pub fn iter(self) -> PositionIter<'a> {
        PositionIter {
            reader: Reader::new(self.payload),
            remaining: self.count,
            previous: 0,
            first: true,
        }
    }
}

pub struct PositionIter<'a> {
    reader: Reader<'a>,
    remaining: u32,
    previous: u32,
    first: bool,
}

impl Iterator for PositionIter<'_> {
    type Item = Result<u32>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let offset = self.reader.offset();
        let next = self.reader.var_u32().and_then(|delta| {
            if !self.first && delta == 0 {
                return Err(Error::new(offset, ErrorKind::InvalidOrder));
            }
            self.previous.checked_add(delta)
                .ok_or(Error::new(offset, ErrorKind::Overflow))
        });
        match next {
            Ok(value) => {
                self.previous = value;
                self.first = false;
                self.remaining -= 1;
            }
            Err(_) => self.remaining = 0,
        }
        Some(next)
    }
}

impl std::iter::FusedIterator for PositionIter<'_> {}

// validates before writing; output size is exact and no allocation occurs.
pub fn encode(positions: &[u32], output: &mut [u8], max_positions: u32) -> Result<usize> {
    let count = u32::try_from(positions.len())
        .map_err(|_| Error::new(0, ErrorKind::LimitExceeded))?;
    if count > max_positions {
        return Err(Error::new(0, ErrorKind::LimitExceeded));
    }
    let mut bytes = 4usize;
    let mut previous = 0;
    for (index, &position) in positions.iter().enumerate() {
        if index != 0 && position <= previous {
            return Err(Error::new(index, ErrorKind::InvalidOrder));
        }
        bytes = bytes.checked_add(var_u32_len(position - previous))
            .ok_or(Error::new(index, ErrorKind::Overflow))?;
        previous = position;
    }
    if output.len() < bytes {
        return Err(Error::new(0, ErrorKind::Truncated));
    }
    let mut writer = Writer::new(output);
    writer.u32(count)?;
    previous = 0;
    for &position in positions {
        writer.var_u32(position - previous)?;
        previous = position;
    }
    Ok(writer.len())
}
