use crate::error::{Error, Result};
use crate::identity::{HeapLayout, RootTid};
use crate::mutable::document::MAX_TERM_BYTES;

const SUFFIX: usize = 1 + 8;
pub const MAX_SORT_RECORD_BYTES: usize = MAX_TERM_BYTES + SUFFIX;

/// one distinct document term in term, root order for postgres bytea sort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TermSortRecord<'a> {
    pub term: &'a str,
    pub root: RootTid,
}

impl TermSortRecord<'_> {
    pub fn encode(self, output: &mut [u8]) -> Result<usize> {
        let term = self.term.as_bytes();
        if term.is_empty() || term.len() > MAX_TERM_BYTES || term.contains(&0) {
            return Err(Error::InvalidParameters);
        }
        let len = term.len() + SUFFIX;
        if output.len() < len {
            return Err(Error::InvalidParameters);
        }
        output[..term.len()].copy_from_slice(term);
        output[term.len()] = 0;
        output[term.len() + 1..term.len() + 9].copy_from_slice(&self.root.key().to_be_bytes());
        Ok(len)
    }
}

pub fn decode_sort_record(input: &[u8], layout: HeapLayout) -> Result<TermSortRecord<'_>> {
    let delimiter = input
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(Error::InvalidState)?;
    if delimiter == 0 || delimiter > MAX_TERM_BYTES || input.len() != delimiter + SUFFIX {
        return Err(Error::InvalidState);
    }
    let term = std::str::from_utf8(&input[..delimiter]).map_err(|_| Error::InvalidState)?;
    let mut root_bytes = [0u8; 8];
    root_bytes.copy_from_slice(&input[delimiter + 1..delimiter + 9]);
    let key = u64::from_be_bytes(root_bytes);
    if key >> 48 != 0 || key >> 16 >= u64::from(u32::MAX) {
        return Err(Error::InvalidState);
    }
    let root =
        RootTid::new((key >> 16) as u32, key as u16, layout).map_err(|_| Error::InvalidState)?;
    Ok(TermSortRecord { term, root })
}
