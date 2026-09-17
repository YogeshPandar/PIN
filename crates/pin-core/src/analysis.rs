// unicode 16 nfc, simple default folding, nfc, then uax #29 word segmentation.
// normalized bytes and token ranges are owned; no locale or hidden stopword state.
// contracts and normalization scratch limits: docs/g1-semantics.md.

use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;
use crate::budget::MemoryBudget;
use crate::error::{Error, Result};
use crate::memory::reserve;

pub const PROFILE_ID: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct AnalysisLimits {
    pub input_bytes: usize,
    pub normalized_bytes: usize,
    pub tokens: u32,
    pub term_bytes: usize,
    pub memory_bytes: usize,
}

impl Default for AnalysisLimits {
    fn default() -> Self {
        Self { input_bytes: 1 << 20, normalized_bytes: 2 << 20, tokens: 262_144, term_bytes: 1024, memory_bytes: 8 << 20 }
    }
}

#[derive(Clone, Copy, Debug)]
struct TokenRange {
    start: usize,
    end: usize,
    position: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token<'a> {
    pub term: &'a str,
    pub position: u32,
}

#[derive(Debug)]
pub struct Analyzed {
    normalized: String,
    tokens: Vec<TokenRange>,
    retained_bytes: usize,
}

impl Analyzed {
    // rejects size, token, allocation and profile errors without returning partial text.
    pub fn analyze(text: &str, limits: AnalysisLimits) -> Result<Self> {
        if unicode_normalization::UNICODE_VERSION != (16, 0, 0)
            || unicode_segmentation::UNICODE_VERSION != (16, 0, 0)
            || unicode_case_mapping::UNICODE_VERSION != (16, 0, 0) {
            return Err(Error::InvalidProfile);
        }
        if text.len() > limits.input_bytes { return Err(Error::Limit("document bytes")); }
        let mut budget = MemoryBudget::new(limits.memory_bytes);
        let mut normalized = Vec::new();
        reserve(&mut normalized, text.len().min(limits.normalized_bytes), limits.normalized_bytes, &mut budget)?;
        let mut invalid_mapping = false;
        let folded = text.nfc().map(|scalar| {
            match unicode_case_mapping::case_folded(scalar) {
                None => scalar,
                Some(mapped) => match char::from_u32(mapped.get()) {
                    Some(value) => value,
                    None => { invalid_mapping = true; scalar }
                },
            }
        });
        for scalar in folded.nfc() {
            let mut bytes = [0; 4];
            let encoded = scalar.encode_utf8(&mut bytes);
            reserve(&mut normalized, encoded.len(), limits.normalized_bytes, &mut budget)?;
            normalized.extend_from_slice(encoded.as_bytes());
        }
        if invalid_mapping { return Err(Error::InvalidProfile); }
        let normalized = String::from_utf8(normalized).map_err(|_| Error::InvalidDocument)?;
        let mut tokens = Vec::new();
        let max_tokens = usize::try_from(limits.tokens).map_err(|_| Error::Limit("tokens"))?;
        for (start, term) in normalized.unicode_word_indices() {
            if term.len() > limits.term_bytes { return Err(Error::Limit("term bytes")); }
            let position = u32::try_from(tokens.len()).map_err(|_| Error::Limit("positions"))?;
            reserve(&mut tokens, 1, max_tokens, &mut budget)?;
            tokens.push(TokenRange { start, end: start + term.len(), position });
        }
        Ok(Self { normalized, tokens, retained_bytes: budget.used() })
    }

    pub const fn profile(&self) -> u32 { PROFILE_ID }
    pub fn len(&self) -> u32 { self.tokens.len() as u32 }
    pub fn is_empty(&self) -> bool { self.tokens.is_empty() }
    pub fn normalized(&self) -> &str { &self.normalized }
    pub const fn retained_bytes(&self) -> usize { self.retained_bytes }

    pub fn token(&self, index: usize) -> Option<Token<'_>> {
        self.tokens.get(index).map(|range| Token {
            term: &self.normalized[range.start..range.end], position: range.position,
        })
    }

    pub fn tokens(&self) -> impl ExactSizeIterator<Item = Token<'_>> {
        self.tokens.iter().map(|range| Token {
            term: &self.normalized[range.start..range.end], position: range.position,
        })
    }
}
