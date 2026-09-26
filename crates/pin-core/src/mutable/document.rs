//! Owned complete-document payloads with exact term frequencies and positions.
//! Preparation borrows analysis output; sorting and allocation precede host locks.
//! Disk readers validate UTF-8, ordering, position uniqueness and byte budgets.
//! Contracts: the G1 position codec and docs/g2-storage.md.

use crate::analysis::{Analyzed, PROFILE_ID, Token};
use crate::budget::MemoryBudget;
use crate::codec::bytes::{Reader, Writer, var_u32_len};
use crate::codec::positions::Positions;
use crate::error::{Error, Result};
use crate::memory::vector;

pub const MAX_TERM_BYTES: usize = 1024;
pub const MAX_DOCUMENT_TOKENS: u32 = 262_144;
pub const MAX_DOCUMENT_BYTES: usize = 8 << 20;
const HEADER: usize = 16;

/// A complete, immutable, Rust-owned representation of one non-null document.
#[derive(Debug)]
pub struct PreparedDocument {
    bytes: Vec<u8>,
    tokens: u32,
    terms: u32,
}

impl PreparedDocument {
    /// Sorts borrowed token references and encodes each term once.
    ///
    /// # Errors
    /// Rejects limits, overflow, allocation failure or insufficient peak budget.
    /// The budget includes the borrowed analyzer's retained allocation.
    pub fn prepare(document: &Analyzed, memory_bytes: usize) -> Result<Self> {
        if document.len() > MAX_DOCUMENT_TOKENS {
            return Err(Error::Limit("document positions"));
        }
        let mut budget = MemoryBudget::new(memory_bytes);
        budget.charge(document.retained_bytes())?;
        let mut tokens: Vec<Token<'_>> = vector(document.len() as usize, &mut budget)?;
        tokens.extend(document.tokens());
        tokens.sort_unstable_by(|a, b| a.term.cmp(b.term).then(a.position.cmp(&b.position)));
        let mut terms = 0u32;
        let mut length = HEADER;
        for group in tokens.chunk_by(|a, b| a.term == b.term) {
            let term = group[0].term;
            if term.is_empty() || term.len() > MAX_TERM_BYTES {
                return Err(Error::Limit("term bytes"));
            }
            terms = terms.checked_add(1).ok_or(Error::Limit("document terms"))?;
            let mut previous = 0;
            let mut position_bytes = 4usize;
            for token in group {
                position_bytes += var_u32_len(token.position - previous);
                previous = token.position;
            }
            length = length
                .checked_add(8 + term.len() + position_bytes)
                .ok_or(Error::Limit("document payload"))?;
        }
        if length > MAX_DOCUMENT_BYTES {
            return Err(Error::Limit("document payload"));
        }
        let mut bytes = vector(length, &mut budget)?;
        bytes.resize(length, 0);
        let mut writer = Writer::new(&mut bytes);
        writer.put(b"PD02")?;
        writer.u32(PROFILE_ID)?;
        writer.u32(document.len())?;
        writer.u32(terms)?;
        for group in tokens.chunk_by(|a, b| a.term == b.term) {
            let term = group[0].term;
            let mut previous = 0;
            let mut position_bytes = 4u32;
            for token in group {
                position_bytes += var_u32_len(token.position - previous) as u32;
                previous = token.position;
            }
            writer.u16(term.len() as u16)?;
            writer.u16(0)?;
            writer.u32(position_bytes)?;
            writer.put(term.as_bytes())?;
            writer.u32(group.len() as u32)?;
            previous = 0;
            for token in group {
                writer.var_u32(token.position - previous)?;
                previous = token.position;
            }
        }
        Ok(Self {
            bytes,
            tokens: document.len(),
            terms,
        })
    }

    /// Copies and validates one complete prepared payload.
    ///
    /// # Errors
    /// Rejects malformed bytes, allocation failure or the supplied memory limit.
    pub fn copy_from_bytes(input: &[u8], memory_bytes: usize) -> Result<Self> {
        let (tokens, terms) = validate(input, memory_bytes)?;
        let mut budget = MemoryBudget::new(memory_bytes);
        let mut bytes = vector(input.len(), &mut budget)?;
        bytes.extend_from_slice(input);
        Ok(Self {
            bytes,
            tokens,
            terms,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn token_count(&self) -> u32 {
        self.tokens
    }

    pub const fn term_count(&self) -> u32 {
        self.terms
    }

    /// Returns a lazy term iterator without decoding positional streams.
    pub fn terms(&self) -> DocumentTerms<'_> {
        DocumentTerms {
            reader: Reader::new(&self.bytes[HEADER..]),
            remaining: self.terms,
        }
    }
}

/// A checked term plus its separate, lazily decoded G1 positional stream.
#[derive(Clone, Copy, Debug)]
pub struct DocumentTerm<'a> {
    pub term: &'a str,
    positions: &'a [u8],
}

impl<'a> DocumentTerm<'a> {
    /// Decodes exact positions, rejecting malformed counts or deltas.
    pub fn positions(self) -> Result<Positions<'a>> {
        Ok(Positions::parse(self.positions, MAX_DOCUMENT_TOKENS)?)
    }
}

pub struct DocumentTerms<'a> {
    reader: Reader<'a>,
    remaining: u32,
}

impl<'a> Iterator for DocumentTerms<'a> {
    type Item = Result<DocumentTerm<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let result = (|| {
            let len = usize::from(self.reader.u16()?);
            if len == 0 || len > MAX_TERM_BYTES || self.reader.u16()? != 0 {
                return Err(Error::InvalidDocument);
            }
            let position_bytes = self.reader.u32()? as usize;
            let term =
                std::str::from_utf8(self.reader.take(len)?).map_err(|_| Error::InvalidDocument)?;
            let positions = self.reader.take(position_bytes)?;
            self.remaining -= 1;
            if self.remaining == 0 {
                self.reader.finish()?;
            }
            Ok(DocumentTerm { term, positions })
        })();
        if result.is_err() {
            self.remaining = 0;
        }
        Some(result)
    }
}

impl std::iter::FusedIterator for DocumentTerms<'_> {}

/// validates one stored payload and returns its zero-copy term iterator.
pub(crate) fn validated_terms(
    bytes: &[u8],
    expected_tokens: u32,
    expected_terms: u32,
    memory_bytes: usize,
) -> Result<DocumentTerms<'_>> {
    let (tokens, terms) = validate(bytes, memory_bytes)?;
    if tokens != expected_tokens || terms != expected_terms {
        return Err(Error::InvalidDocument);
    }
    Ok(DocumentTerms {
        reader: Reader::new(&bytes[HEADER..]),
        remaining: terms,
    })
}

/// proves one bounded phrase against the stored, complete positional payload.
/// the caller still needs the heap AM to check MVCC visibility.
pub fn phrase_matches(
    bytes: &[u8],
    expected_tokens: u32,
    expected_terms: u32,
    phrase: &[String],
    memory_bytes: usize,
) -> Result<bool> {
    if phrase.is_empty() || phrase.len() > 64 {
        return Err(Error::Limit("phrase terms"));
    }
    let mut positions: [Option<Positions<'_>>; 64] = [None; 64];
    for entry in validated_terms(bytes, expected_tokens, expected_terms, memory_bytes)? {
        let entry = entry?;
        if phrase.iter().any(|term| term == entry.term) {
            let view = entry.positions()?;
            for (index, term) in phrase.iter().enumerate() {
                if term == entry.term {
                    positions[index] = Some(view);
                }
            }
        }
    }
    let Some(first) = positions[0] else {
        return Ok(false);
    };
    if positions[..phrase.len()].iter().any(Option::is_none) {
        return Ok(false);
    }
    let mut cursors =
        core::array::from_fn::<_, 64, _>(|index| positions[index].map(Positions::iter));
    let mut current = [None; 64];
    for start in first.iter() {
        let start = start?;
        let mut matched = true;
        for index in 1..phrase.len() {
            let Some(target) = start.checked_add(index as u32) else {
                return Ok(false);
            };
            let cursor = cursors[index].as_mut().ok_or(Error::InvalidState)?;
            loop {
                let value = match current[index] {
                    Some(value) => Some(value),
                    None => cursor.next().transpose()?,
                };
                current[index] = value;
                match value {
                    Some(value) if value < target => current[index] = None,
                    Some(value) if value == target => break,
                    _ => {
                        matched = false;
                        break;
                    }
                }
            }
            if !matched {
                break;
            }
        }
        if matched {
            return Ok(true);
        }
    }
    Ok(false)
}

/// query-constant ordering for exact boolean term membership.
pub(crate) struct TermMembership<'a, 'b> {
    names: &'a [Option<&'b str>],
    order: [u8; 64],
    ordered: usize,
}

impl<'a, 'b> TermMembership<'a, 'b> {
    pub(crate) fn new(names: &'a [Option<&'b str>]) -> Result<Self> {
        if names.len() > 64 {
            return Err(Error::Limit("document membership"));
        }
        let mut order = [0u8; 64];
        let mut ordered = 0usize;
        for (index, name) in names.iter().enumerate() {
            let Some(name) = *name else {
                continue;
            };
            let mut position = ordered;
            while position != 0 {
                let previous = usize::from(order[position - 1]);
                let previous_name = names[previous].ok_or(Error::InvalidState)?;
                if previous_name <= name {
                    break;
                }
                order[position] = order[position - 1];
                position -= 1;
            }
            order[position] = index as u8;
            ordered += 1;
        }
        Ok(Self {
            names,
            order,
            ordered,
        })
    }

    /// reads exact term membership without decoding unused positional deltas.
    pub(crate) fn read(
        &self,
        bytes: &[u8],
        expected_tokens: u32,
        expected_terms: u32,
    ) -> Result<u64> {
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(Error::Limit("document membership"));
        }
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != b"PD02" || reader.u32()? != PROFILE_ID {
            return Err(Error::InvalidProfile);
        }
        let tokens = reader.u32()?;
        let terms = reader.u32()?;
        if tokens != expected_tokens
            || terms != expected_terms
            || tokens > MAX_DOCUMENT_TOKENS
            || terms > tokens
        {
            return Err(Error::InvalidDocument);
        }

        let mut membership = 0u64;
        let mut query = 0usize;
        let mut previous = None;
        let mut positions = 0u32;
        for _ in 0..terms {
            let len = usize::from(reader.u16()?);
            if len == 0 || len > MAX_TERM_BYTES || reader.u16()? != 0 {
                return Err(Error::InvalidDocument);
            }
            let position_bytes = reader.u32()? as usize;
            let term =
                std::str::from_utf8(reader.take(len)?).map_err(|_| Error::InvalidDocument)?;
            if previous.is_some_and(|previous| previous >= term) {
                return Err(Error::InvalidDocument);
            }
            previous = Some(term);
            let mut encoded = Reader::new(reader.take(position_bytes)?);
            let count = encoded.u32()?;
            if count == 0 || count > tokens || count as usize > encoded.remaining() {
                return Err(Error::InvalidDocument);
            }
            positions = positions.checked_add(count).ok_or(Error::InvalidDocument)?;

            while query < self.ordered {
                let index = usize::from(self.order[query]);
                let name = self.names[index].ok_or(Error::InvalidState)?;
                if name < term {
                    query += 1;
                    continue;
                }
                if name != term {
                    break;
                }
                membership |= 1u64 << index;
                query += 1;
            }
        }
        reader.finish()?;
        if positions != tokens {
            return Err(Error::InvalidDocument);
        }
        Ok(membership)
    }
}

/// Validates an untrusted payload without retaining a decoded document.
///
/// # Errors
/// Rejects malformed bytes, duplicate or missing positions, profile mismatch,
/// noncanonical ordering and resource limits. Scratch is at most one position bitset.
pub fn validate(bytes: &[u8], memory_bytes: usize) -> Result<(u32, u32)> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::Limit("document payload"));
    }
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != b"PD02" || reader.u32()? != PROFILE_ID {
        return Err(Error::InvalidProfile);
    }
    let tokens = reader.u32()?;
    let terms = reader.u32()?;
    if tokens > MAX_DOCUMENT_TOKENS || terms > tokens {
        return Err(Error::InvalidDocument);
    }
    if terms == 0 {
        reader.finish()?;
        return if tokens == 0 {
            Ok((0, 0))
        } else {
            Err(Error::InvalidDocument)
        };
    }
    let mut budget = MemoryBudget::new(memory_bytes);
    let words = (tokens as usize).div_ceil(64);
    let mut seen: Vec<u64> = vector(words, &mut budget)?;
    seen.resize(words, 0);
    let iter = DocumentTerms {
        reader,
        remaining: terms,
    };
    let mut previous = "";
    let mut total = 0u32;
    for entry in iter {
        let entry = entry?;
        if entry.term <= previous {
            return Err(Error::InvalidDocument);
        }
        previous = entry.term;
        let positions = entry.positions()?;
        if positions.is_empty() {
            return Err(Error::InvalidDocument);
        }
        for position in positions.iter() {
            let position = position?;
            if position >= tokens {
                return Err(Error::InvalidDocument);
            }
            let word = &mut seen[position as usize / 64];
            let mask = 1u64 << (position % 64);
            if *word & mask != 0 {
                return Err(Error::InvalidDocument);
            }
            *word |= mask;
            total += 1;
        }
    }
    if total != tokens {
        return Err(Error::InvalidDocument);
    }
    Ok((tokens, terms))
}
