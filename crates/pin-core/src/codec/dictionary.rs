// borrowed sorted terms with constant-time offset access and logarithmic lookup.
// term ids are dictionary-local; profile identity does not prove normalization.

use super::bytes::Reader;
use super::records::{HEADER_BYTES, payload, start};
use super::{Error, ErrorKind, Result};
use std::ops::Range;

#[derive(Clone, Copy, Debug)]
pub struct DictionaryLimits {
    pub max_bytes: usize,
    pub max_terms: u32,
    pub max_term_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TermId(pub u32);

#[derive(Clone, Copy, Debug)]
pub struct Dictionary<'a> {
    profile: u32,
    offsets: &'a [u8],
    terms: &'a [u8],
    count: u32,
}

impl<'a> Dictionary<'a> {
    // validates every offset, term, ordering relation and trailing byte before use.
    pub fn parse(bytes: &'a [u8], limits: DictionaryLimits) -> Result<Self> {
        let mut reader = Reader::new(payload(bytes, 3, limits.max_bytes)?);
        let profile = reader.u32()?;
        if profile == 0 {
            return Err(Error::new(0, ErrorKind::InvalidValue));
        }
        let count = reader.u32()?;
        if count > limits.max_terms {
            return Err(Error::new(4, ErrorKind::LimitExceeded));
        }
        let offset_bytes = usize::try_from(count)
            .ok()
            .and_then(|n| n.checked_add(1))
            .and_then(|n| n.checked_mul(4))
            .ok_or(Error::new(8, ErrorKind::Overflow))?;
        let offsets = reader.take(offset_bytes)?;
        let terms = reader.take(reader.remaining())?;
        let view = Self {
            profile,
            offsets,
            terms,
            count,
        };
        if view.offset(0)? != 0 || view.offset(count)? != terms.len() {
            return Err(Error::new(8, ErrorKind::InvalidValue));
        }
        let mut previous = "";
        for index in 0..count {
            let term = view.term(TermId(index))?;
            if term.is_empty() || term.len() > limits.max_term_bytes {
                return Err(Error::new(8, ErrorKind::LimitExceeded));
            }
            if term <= previous {
                return Err(Error::new(8, ErrorKind::InvalidOrder));
            }
            previous = term;
        }
        Ok(view)
    }

    pub const fn profile(self) -> u32 {
        self.profile
    }
    pub const fn len(self) -> u32 {
        self.count
    }
    pub const fn is_empty(self) -> bool {
        self.count == 0
    }

    fn offset(self, index: u32) -> Result<usize> {
        let offset = usize::try_from(index)
            .ok()
            .and_then(|n| n.checked_mul(4))
            .ok_or(Error::new(8, ErrorKind::Overflow))?;
        let bytes = self
            .offsets
            .get(offset..)
            .ok_or(Error::new(8, ErrorKind::Truncated))?;
        usize::try_from(Reader::new(bytes).u32()?).map_err(|_| Error::new(8, ErrorKind::Overflow))
    }

    pub fn term(self, id: TermId) -> Result<&'a str> {
        if id.0 >= self.count {
            return Err(Error::new(8, ErrorKind::InvalidValue));
        }
        let start = self.offset(id.0)?;
        let end = self.offset(id.0 + 1)?;
        let bytes = self
            .terms
            .get(start..end)
            .ok_or(Error::new(8, ErrorKind::InvalidValue))?;
        std::str::from_utf8(bytes).map_err(|_| Error::new(start, ErrorKind::InvalidUtf8))
    }

    fn lower_bound(self, term: &str) -> Result<u32> {
        let mut low = 0;
        let mut high = self.count;
        while low < high {
            let middle = low + (high - low) / 2;
            if self.term(TermId(middle))? < term {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }

    pub fn lookup(self, term: &str) -> Result<Option<TermId>> {
        let index = self.lower_bound(term)?;
        if index < self.count && self.term(TermId(index))? == term {
            Ok(Some(TermId(index)))
        } else {
            Ok(None)
        }
    }

    // locates the complete interval without expanding terms or constructing a successor key.
    pub fn prefix(self, prefix: &str, max_expanded: u32) -> Result<Range<u32>> {
        let begin = self.lower_bound(prefix)?;
        let mut low = begin;
        let mut high = self.count;
        while low < high {
            let middle = low + (high - low) / 2;
            if self.term(TermId(middle))?.starts_with(prefix) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low - begin > max_expanded {
            return Err(Error::new(0, ErrorKind::LimitExceeded));
        }
        Ok(begin..low)
    }
}

pub fn encode(
    profile: u32,
    terms: &[&str],
    output: &mut [u8],
    limits: DictionaryLimits,
) -> Result<usize> {
    if profile == 0 {
        return Err(Error::new(0, ErrorKind::InvalidValue));
    }
    let count = u32::try_from(terms.len()).map_err(|_| Error::new(4, ErrorKind::LimitExceeded))?;
    if count > limits.max_terms {
        return Err(Error::new(4, ErrorKind::LimitExceeded));
    }
    let mut data_bytes = 0usize;
    let mut previous = "";
    for &term in terms {
        if term.is_empty() || term.len() > limits.max_term_bytes {
            return Err(Error::new(0, ErrorKind::LimitExceeded));
        }
        if term <= previous {
            return Err(Error::new(0, ErrorKind::InvalidOrder));
        }
        data_bytes = data_bytes
            .checked_add(term.len())
            .ok_or(Error::new(0, ErrorKind::Overflow))?;
        previous = term;
    }
    let data_bytes = u32::try_from(data_bytes).map_err(|_| Error::new(0, ErrorKind::Overflow))?;
    let len = terms
        .len()
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| n.checked_add(8))
        .and_then(|n| n.checked_add(data_bytes as usize))
        .ok_or(Error::new(0, ErrorKind::Overflow))?;
    if len
        .checked_add(HEADER_BYTES)
        .is_none_or(|total| total > limits.max_bytes)
    {
        return Err(Error::new(0, ErrorKind::LimitExceeded));
    }
    let mut writer = start(output, 3, len)?;
    writer.u32(profile)?;
    writer.u32(count)?;
    let mut offset = 0u32;
    writer.u32(offset)?;
    for &term in terms {
        offset += term.len() as u32;
        writer.u32(offset)?;
    }
    for &term in terms {
        writer.put(term.as_bytes())?;
    }
    Ok(writer.len())
