// unicode 16 nfc, simple default folding, nfc, then uax #29 word segmentation.
// normalized bytes and token ranges are owned; no locale or hidden stopword state.
// contracts and normalization scratch limits: docs/g1-semantics.md.

use crate::budget::MemoryBudget;
use crate::error::{Error, Result};
use crate::memory::reserve;
use crate::normalize::profile_text;
use unicode_segmentation::UnicodeSegmentation;

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
        Self {
            input_bytes: 1 << 20,
            normalized_bytes: 2 << 20,
            tokens: 262_144,
            term_bytes: 1024,
            memory_bytes: 8 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TokenRange {
    start: u32,
    end: u32,
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
    peak_bytes: usize,
}

impl Analyzed {
    // rejects size, token, allocation and profile errors without returning partial text.
    pub fn analyze(text: &str, limits: AnalysisLimits) -> Result<Self> {
        if unicode_normalization::UNICODE_VERSION != (16, 0, 0)
            || unicode_segmentation::UNICODE_VERSION != (16, 0, 0)
            || unicode_case_mapping::UNICODE_VERSION != (16, 0, 0)
        {
            return Err(Error::InvalidProfile);
        }
        if text.len() > limits.input_bytes {
            return Err(Error::Limit("document bytes"));
        }
        let mut budget = MemoryBudget::new(limits.memory_bytes);
        let (normalized, peak) = profile_text(text, limits.normalized_bytes, &mut budget)?;
        u32::try_from(normalized.len()).map_err(|_| Error::Limit("normalized bytes"))?;
        let mut tokens = Vec::new();
        let max_tokens = usize::try_from(limits.tokens).map_err(|_| Error::Limit("tokens"))?;
        for (start, term) in normalized.unicode_word_indices() {
            if term.len() > limits.term_bytes {
                return Err(Error::Limit("term bytes"));
            }
            reserve(&mut tokens, 1, max_tokens, &mut budget)?;
            tokens.push(TokenRange {
                start: start as u32,
                end: (start + term.len()) as u32,
            });
        }
        Ok(Self {
            normalized,
            tokens,
            retained_bytes: budget.used(),
            peak_bytes: peak.max(budget.used()),
        })
    }

    pub const fn profile(&self) -> u32 {
        PROFILE_ID
    }
    pub fn len(&self) -> u32 {
        self.tokens.len() as u32
    }
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
    pub fn normalized(&self) -> &str {
        &self.normalized
    }
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub const fn peak_bytes(&self) -> usize { self.peak_bytes }

    pub fn token(&self, index: usize) -> Option<Token<'_>> {
        self.tokens.get(index).map(|range| Token {
            term: &self.normalized[range.start as usize..range.end as usize],
            position: index as u32,
        })
    }

    pub fn tokens(&self) -> impl ExactSizeIterator<Item = Token<'_>> {
        self.tokens.iter().enumerate().map(|(index, range)| Token {
            term: &self.normalized[range.start as usize..range.end as usize],
            position: index as u32,
        })
    }
}
