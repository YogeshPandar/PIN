// adaptive sparse, bitmap and run encodings over a validated heap-offset domain.
// set operations use fixed scratch; subtraction never invents universe members.
// contract: https://doc.rust-lang.org/std/primitive.u64.html#method.trailing_zeros

use super::bytes::{Reader, Writer};
use super::{Error, ErrorKind, Result};
use crate::identity::HeapLayout;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Encoding {
    Sparse,
    Bitmap,
    Runs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OffsetSet {
    words: [u64; 8],
    domain: u16,
}

impl OffsetSet {
    pub fn new(layout: HeapLayout) -> Self {
        Self {
            words: [0; 8],
            domain: layout.max_offset(),
        }
    }

    pub fn len(&self) -> u16 {
        self.words.iter().map(|word| word.count_ones() as u16).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&word| word == 0)
    }

    pub fn contains(&self, offset: u16) -> bool {
        if offset == 0 || offset > self.domain {
            return false;
        }
        let bit = usize::from(offset - 1);
        self.words[bit / 64] & (1u64 << (bit % 64)) != 0
    }

    // rejects out-of-domain offsets before touching the bitmap.
    pub fn insert(&mut self, offset: u16) -> Result<bool> {
        if offset == 0 || offset > self.domain {
            return Err(Error::new(0, ErrorKind::InvalidValue));
        }
        let bit = usize::from(offset - 1);
        let mask = 1u64 << (bit % 64);
        let word = &mut self.words[bit / 64];
        let fresh = *word & mask == 0;
        *word |= mask;
        Ok(fresh)
    }

    pub fn iter(&self) -> OffsetIter<'_> {
        OffsetIter {
            words: &self.words,
            word: 0,
            pending: self.words[0],
        }
    }

    fn combine(&self, other: &Self, op: fn(u64, u64) -> u64) -> Result<Self> {
        if self.domain != other.domain {
            return Err(Error::new(0, ErrorKind::InvalidValue));
        }
        let mut result = self.clone();
        for (index, word) in result.words.iter_mut().enumerate() {
            *word = op(self.words[index], other.words[index]);
        }
        Ok(result)
    }

    pub fn intersection(&self, other: &Self) -> Result<Self> {
        self.combine(other, |left, right| left & right)
    }

    pub fn union(&self, other: &Self) -> Result<Self> {
        self.combine(other, |left, right| left | right)
    }

    pub fn difference(&self, other: &Self) -> Result<Self> {
        self.combine(other, |left, right| left & !right)
    }

    fn run_count(&self) -> u16 {
        let mut runs = 0;
        let mut previous = 0;
        for offset in self.iter() {
            if previous == 0 || offset != previous + 1 {
                runs += 1;
            }
            previous = offset;
        }
        runs
    }

    pub fn encoded_len(&self, encoding: Encoding) -> usize {
        match encoding {
            Encoding::Sparse => 6 + usize::from(self.len()) * 2,
            Encoding::Bitmap => 6 + usize::from(self.domain).div_ceil(8),
            Encoding::Runs => 8 + usize::from(self.run_count()) * 4,
        }
    }

    // ties prefer sparse, then bitmap, then runs; headers count toward selection.
    pub fn preferred_encoding(&self) -> Encoding {
        let mut best = Encoding::Sparse;
        for encoding in [Encoding::Bitmap, Encoding::Runs] {
            if self.encoded_len(encoding) < self.encoded_len(best) {
                best = encoding;
            }
        }
        best
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize> {
        self.encode_as(self.preferred_encoding(), output)
    }

    pub fn encode_as(&self, encoding: Encoding, output: &mut [u8]) -> Result<usize> {
        if output.len() < self.encoded_len(encoding) {
            return Err(Error::new(0, ErrorKind::Truncated));
        }
        let mut writer = Writer::new(output);
        writer.u16(self.domain)?;
        writer.u16(self.len())?;
        writer.u8(match encoding {
            Encoding::Sparse => 0,
            Encoding::Bitmap => 1,
            Encoding::Runs => 2,
        })?;
        writer.u8(0)?;
        match encoding {
            Encoding::Sparse => {
                for offset in self.iter() {
                    writer.u16(offset)?;
                }
            }
            Encoding::Bitmap => {
                for byte in 0..usize::from(self.domain).div_ceil(8) {
                    writer.u8((self.words[byte / 8] >> ((byte % 8) * 8)) as u8)?;
                }
            }
            Encoding::Runs => {
                writer.u16(self.run_count())?;
                let mut offsets = self.iter().peekable();
                while let Some(start) = offsets.next() {
                    let mut end = start;
                    while offsets.peek().copied() == Some(end + 1) {
                        offsets.next();
                        end += 1;
                    }
                    writer.u16(start)?;
                    writer.u16(end - start + 1)?;
                }
            }
        }
        Ok(writer.len())
    }

    // rejects invalid cardinality, ordering, reserved bits and bitmap tails.
    pub fn parse(bytes: &[u8], layout: HeapLayout) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        if reader.u16()? != layout.max_offset() {
            return Err(Error::new(0, ErrorKind::InvalidValue));
        }
        let count = reader.u16()?;
        if count > layout.max_offset() {
            return Err(Error::new(2, ErrorKind::InvalidValue));
        }
        let tag = reader.u8()?;
        if reader.u8()? != 0 {
            return Err(Error::new(5, ErrorKind::NonCanonical));
        }
        let mut result = Self::new(layout);
        match tag {
            0 => {
                let mut previous = 0;
                for _ in 0..count {
                    let offset = reader.u16()?;
                    if offset <= previous {
                        return Err(Error::new(reader.offset() - 2, ErrorKind::InvalidOrder));
                    }
                    result.insert(offset)?;
                    previous = offset;
                }
            }
            1 => {
                let bytes = reader.take(usize::from(result.domain).div_ceil(8))?;
                for (index, &byte) in bytes.iter().enumerate() {
                    result.words[index / 8] |= u64::from(byte) << ((index % 8) * 8);
                }
                let tail = result.domain % 8;
                if tail != 0 && bytes.last().is_some_and(|byte| byte >> tail != 0) {
                    return Err(Error::new(reader.offset() - 1, ErrorKind::NonCanonical));
                }
            }
            2 => {
                let runs = reader.u16()?;
                if runs > count {
                    return Err(Error::new(6, ErrorKind::InvalidValue));
                }
                let mut previous = 0u16;
                for _ in 0..runs {
                    let start = reader.u16()?;
                    let len = reader.u16()?;
                    if start == 0 || len == 0 || (previous != 0 && start <= previous + 1) {
                        return Err(Error::new(reader.offset() - 4, ErrorKind::InvalidOrder));
                    }
                    let end = start
                        .checked_add(len - 1)
                        .ok_or(Error::new(reader.offset() - 4, ErrorKind::Overflow))?;
                    if end > result.domain {
                        return Err(Error::new(reader.offset() - 4, ErrorKind::InvalidValue));
                    }
                    for offset in start..=end {
                        result.insert(offset)?;
                    }
                    previous = end;
                }
            }
            _ => return Err(Error::new(4, ErrorKind::UnknownTag)),
        }
        reader.finish()?;
        if result.len() != count {
            return Err(Error::new(2, ErrorKind::InvalidValue));
        }
        Ok(result)
    }
}

pub struct OffsetIter<'a> {
    words: &'a [u64; 8],
    word: usize,
    pending: u64,
}

impl Iterator for OffsetIter<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if self.word >= self.words.len() {
            return None;
        }
        while self.pending == 0 {
            self.word += 1;
            self.pending = *self.words.get(self.word)?;
        }
        let bit = self.pending.trailing_zeros() as usize;
        self.pending &= self.pending - 1;
        Some((self.word * 64 + bit + 1) as u16)
    }
}

impl std::iter::FusedIterator for OffsetIter<'_> {}
