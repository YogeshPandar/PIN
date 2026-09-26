//! Owned complete-document payloads with exact term frequencies and positions.
//! Preparation borrows analysis output; sorting and allocation precede host locks.
//! Disk readers validate UTF-8, ordering, position uniqueness and byte budgets.
//! Contracts: the G1 position codec and docs/g2-storage.md.

use crate::analysis::{Analyzed, PROFILE_ID, Token};
use crate::budget::MemoryBudget;
use crate::codec::bytes::{Reader, Writer, var_u32_len};
#[path = "document_positions.rs"]
mod streams;
use crate::error::{Error, Result};
use crate::memory::vector;
use DocumentPositions as Positions;
pub use streams::{DocumentPositionIter, DocumentPositions};

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
        Self::prepare_format(document, memory_bytes, false)
    }

    /// Experimental PD03 encoding; native storage insertion remains gated.
    pub fn prepare_blocked(document: &Analyzed, memory_bytes: usize) -> Result<Self> {
        Self::prepare_format(document, memory_bytes, true)
    }

    fn prepare_format(document: &Analyzed, memory_bytes: usize, blocked: bool) -> Result<Self> {
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
        let mut largest_blocked = 0;
        let mut largest_payload = 0;
        for group in tokens.chunk_by(|a, b| a.term == b.term) {
            let term = group[0].term;
            if term.is_empty() || term.len() > MAX_TERM_BYTES {
                return Err(Error::Limit("term bytes"));
            }
            terms = terms.checked_add(1).ok_or(Error::Limit("document terms"))?;
            let use_blocks = blocked && group.len() >= 256;
            let position_bytes = position_bytes(group, use_blocks);
            if use_blocks {
                largest_blocked = largest_blocked.max(group.len());
                largest_payload = largest_payload.max(position_bytes);
            }
            length = length
                .checked_add(8 + term.len() + position_bytes)
                .ok_or(Error::Limit("document payload"))?;
        }
        if length > MAX_DOCUMENT_BYTES {
            return Err(Error::Limit("document payload"));
        }
        // keep the established representation when no stream can use blocks.
        let blocked = blocked && largest_blocked != 0;
        let mut block_values: Vec<u32> = vector(largest_blocked, &mut budget)?;
        let mut block_bytes: Vec<u8> = vector(largest_payload, &mut budget)?;
        block_bytes.resize(largest_payload, 0);
        let mut bytes = vector(length, &mut budget)?;
        bytes.resize(length, 0);
        let mut writer = Writer::new(&mut bytes);
        writer.put(if blocked { b"PD03" } else { b"PD02" })?;
        writer.u32(PROFILE_ID)?;
        writer.u32(document.len())?;
        writer.u32(terms)?;
        for group in tokens.chunk_by(|a, b| a.term == b.term) {
            let term = group[0].term;
            let use_blocks = blocked && group.len() >= 256;
            let position_bytes = position_bytes(group, use_blocks);
            writer.u16(term.len() as u16)?;
            writer.u16(u16::from(use_blocks))?;
            writer.u32(position_bytes as u32)?;
            writer.put(term.as_bytes())?;
            if use_blocks {
                block_values.clear();
                block_values.extend(group.iter().map(|token| token.position));
                let written = crate::codec::position_blocks::encode(
                    &block_values,
                    &mut block_bytes,
                    MAX_DOCUMENT_TOKENS,
                )?;
                if written != position_bytes {
                    return Err(Error::InvalidDocument);
                }
                writer.put(&block_bytes[..written])?;
            } else {
                writer.u32(group.len() as u32)?;
                let mut previous = 0;
                for token in group {
                    writer.var_u32(token.position - previous)?;
                    previous = token.position;
                }
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

    /// True when a PD02 document has a term large enough for PB01 blocks.
    pub fn has_block_candidate(&self) -> Result<bool> {
        if self.bytes.starts_with(b"PD03") {
            return Ok(true);
        }
        for term in self.terms() {
            let term = term?;
            let count = Reader::new(term.positions).u32()?;
            if count >= 256 {
                return Ok(true);
            }
        }
        Ok(false)
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
            blocked_document: self.bytes.starts_with(b"PD03"),
        }
    }
}

/// A checked term plus its separate, lazily decoded G1 positional stream.
#[derive(Clone, Copy, Debug)]
pub struct DocumentTerm<'a> {
    pub term: &'a str,
    positions: &'a [u8],
    blocked: bool,
}

impl<'a> DocumentTerm<'a> {
    /// Decodes exact positions, rejecting malformed counts or deltas.
    pub fn positions(self) -> Result<Positions<'a>> {
        Positions::parse(self.positions, self.blocked)
    }
}

pub struct DocumentTerms<'a> {
    reader: Reader<'a>,
    remaining: u32,
    blocked_document: bool,
}

impl<'a> Iterator for DocumentTerms<'a> {
    type Item = Result<DocumentTerm<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let result = (|| {
            let len = usize::from(self.reader.u16()?);
            let encoding = self.reader.u16()?;
            if len == 0 || len > MAX_TERM_BYTES || encoding > u16::from(self.blocked_document) {
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
            Ok(DocumentTerm {
                term,
                positions,
                blocked: encoding == 1,
            })
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
        blocked_document: bytes.starts_with(b"PD03"),
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
    let (tokens, terms) = validate_inner(bytes, memory_bytes, |term, view| {
        for (index, wanted) in phrase.iter().enumerate() {
            if wanted == term {
                positions[index] = Some(view);
            }
        }
        Ok(())
    })?;
    if tokens != expected_tokens || terms != expected_terms {
        return Err(Error::InvalidDocument);
    }
    match_phrase(&positions[..phrase.len()])
}

/// selected views borrow one complete payload; unused position deltas stay encoded.
/// full cross-term position uniqueness is checked by `validate`, not this reader.
pub struct SelectedPositions<'a> {
    positions: [Option<Positions<'a>>; 64],
    selected: usize,
    pub directory_terms: u32,
    pub selected_positions: u32,
}

impl<'a> SelectedPositions<'a> {
    /// validates the directory and selected streams, without a document-sized bitset.
    pub fn read(
        bytes: &'a [u8],
        expected_tokens: u32,
        expected_terms: u32,
        wanted: &[String],
    ) -> Result<Self> {
        if wanted.is_empty() || wanted.len() > 64 || bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(Error::Limit("selected positions"));
        }
        let mut reader = Reader::new(bytes);
        let blocked_document = read_profile(&mut reader)?;
        let tokens = reader.u32()?;
        let terms = reader.u32()?;
        if tokens != expected_tokens
            || terms != expected_terms
            || tokens > MAX_DOCUMENT_TOKENS
            || terms > tokens
        {
            return Err(Error::InvalidDocument);
        }
        if terms == 0 {
            reader.finish()?;
        }
        let mut result = Self {
            positions: [None; 64],
            selected: wanted.len(),
            directory_terms: terms,
            selected_positions: 0,
        };
        let mut previous = "";
        let mut total = 0u32;
        for entry in (DocumentTerms {
            reader,
            remaining: terms,
            blocked_document,
        }) {
            let entry = entry?;
            if entry.term <= previous {
                return Err(Error::InvalidDocument);
            }
            previous = entry.term;
            let count = Positions::count(entry.positions, entry.blocked, tokens)?;
            total = total.checked_add(count).ok_or(Error::InvalidDocument)?;
            if !wanted.iter().any(|name| name == entry.term) {
                continue;
            }
            let view = entry.positions()?;
            for position in view.iter() {
                if position? >= tokens {
                    return Err(Error::InvalidDocument);
                }
            }
            result.selected_positions = result
                .selected_positions
                .checked_add(count)
                .ok_or(Error::InvalidDocument)?;
            for (index, name) in wanted.iter().enumerate() {
                if name == entry.term {
                    result.positions[index] = Some(view);
                }
            }
        }
        if total != tokens {
            return Err(Error::InvalidDocument);
        }
        Ok(result)
    }

    pub fn phrase_matches(&self) -> Result<bool> {
        match_phrase(&self.positions[..self.selected])
    }
}

fn match_phrase(positions: &[Option<Positions<'_>>]) -> Result<bool> {
    if positions.iter().any(Option::is_none) {
        return Ok(false);
    }
    let anchor = positions
        .iter()
        .enumerate()
        .min_by_key(|(_, view)| view.map_or(u32::MAX, Positions::len))
        .map(|(index, _)| index)
        .ok_or(Error::InvalidState)?;
    let first = positions[anchor].ok_or(Error::InvalidState)?;
    let mut cursors = [const { None }; 64];
    for index in 0..positions.len() {
        cursors[index] = positions[index].map(Positions::iter);
    }
    let mut current = [None; 64];
    for position in first.iter() {
        let Some(start) = position?.checked_sub(anchor as u32) else {
            continue;
        };
        let mut matched = true;
        for index in 0..positions.len() {
            if index == anchor {
                continue;
            }
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
        let blocked_document = read_profile(&mut reader)?;
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
            let encoding = reader.u16()?;
            if len == 0 || len > MAX_TERM_BYTES || encoding > u16::from(blocked_document) {
                return Err(Error::InvalidDocument);
            }
            let position_bytes = reader.u32()? as usize;
            let term =
                std::str::from_utf8(reader.take(len)?).map_err(|_| Error::InvalidDocument)?;
            if previous.is_some_and(|previous| previous >= term) {
                return Err(Error::InvalidDocument);
            }
            previous = Some(term);
            let count = Positions::count(reader.take(position_bytes)?, encoding == 1, tokens)?;
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
    validate_inner(bytes, memory_bytes, |_, _| Ok(()))
}

fn validate_inner<'a>(
    bytes: &'a [u8],
    memory_bytes: usize,
    mut on_term: impl FnMut(&'a str, Positions<'a>) -> Result<()>,
) -> Result<(u32, u32)> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::Limit("document payload"));
    }
    let mut reader = Reader::new(bytes);
    let _blocked_document = read_profile(&mut reader)?;
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
        blocked_document: bytes.starts_with(b"PD03"),
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
        on_term(entry.term, positions)?;
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

fn read_profile(reader: &mut Reader<'_>) -> Result<bool> {
    let magic = reader.take(4)?;
    if (magic != b"PD02" && magic != b"PD03") || reader.u32()? != PROFILE_ID {
        return Err(Error::InvalidProfile);
    }
    Ok(magic == b"PD03")
}

// bounded by MAX_DOCUMENT_TOKENS and the analyzer's sorted positional domain.
fn position_bytes(group: &[Token<'_>], blocked: bool) -> usize {
    let mut size = if blocked {
        8 + group.len().div_ceil(128) * 16
    } else {
        4
    };
    let mut previous = 0;
    for (index, token) in group.iter().enumerate() {
        if !blocked || index % 128 != 0 {
            size += var_u32_len(token.position - previous);
        }
        previous = token.position;
    }
    size
}

#[cfg(test)]
mod blocked_membership_tests {
    use super::*;
    use crate::analysis::AnalysisLimits;

    #[test]
    fn membership_and_validated_terms_read_both_formats() {
        let text = format!("{}zulu", "alpha beta ".repeat(300));
        let analyzed = Analyzed::analyze(&text, AnalysisLimits::default()).unwrap();
        let names = [Some("alpha"), Some("missing"), Some("zulu"), Some("alpha")];
        let query = TermMembership::new(&names).unwrap();
        for blocked in [false, true] {
            let doc = PreparedDocument::prepare_format(&analyzed, 1 << 20, blocked).unwrap();
            assert_eq!(query.read(doc.bytes(), doc.tokens, doc.terms).unwrap(), 13);
            let names: Vec<_> = validated_terms(doc.bytes(), doc.tokens, doc.terms, 1 << 20)
                .unwrap()
                .map(|term| term.unwrap().term.to_owned())
                .collect();
            assert_eq!(names, ["alpha", "beta", "zulu"]);
        }
    }
}
