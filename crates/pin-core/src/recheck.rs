//! exact single-term and bounded phrase rechecks without document token buffers.
//! matchers borrow one validated query; text borrows end before each call returns.
//! ASCII uses borrowed text. Unicode uses the existing budgeted normalizer.
//! contracts and the default-off PostgreSQL integration: docs/g6-optimization.md.

use crate::analysis::{AnalysisLimits, check_input};
use crate::budget::MemoryBudget;
use crate::error::{Error, Result};
use crate::memory::Work;
use crate::normalize::profile_text;
use crate::query::{Kind, Query};
use unicode_segmentation::UnicodeSegmentation;

/// borrows one canonical query term; no allocation or document state is retained.
#[derive(Clone, Copy, Debug)]
pub struct SingleTermMatcher<'q> {
    term: &'q str,
}

impl<'q> SingleTermMatcher<'q> {
    /// accepts exactly one term node; other shapes keep the document oracle.
    /// no allocation occurs, and the matcher cannot outlive the query.
    pub fn new(query: &'q Query) -> Option<Self> {
        if query.node_count() != 1 {
            return None;
        }
        match &query.nodes[query.root].kind {
            Kind::Term(term) => Some(Self { term }),
            _ => None,
        }
    }

    /// checks the full document, then returns exact term membership.
    ///
    /// ASCII needs no owned buffer. Unicode normalization charges actual
    /// capacity against `limits.memory_bytes`; all scratch is released on return.
    /// Borrowed input and the separately owned query are not charged again.
    ///
    /// # Errors
    /// rejects profile, input, normalized-byte, term and token limits, including
    /// invalid tails after a match. Search-work accounting equals the single-node
    /// oracle: one node plus tokens through the first match, or every token when
    /// absent. Analysis errors precede search-work errors, as in the oracle.
    /// Allocation/budget failures from Unicode normalization are propagated.
    pub fn matches(&self, text: &str, limits: AnalysisLimits, max_steps: usize) -> Result<bool> {
        check_input(text, limits)?;
        if text.is_ascii() {
            if text.len() > limits.normalized_bytes {
                return Err(Error::Limit("normalized bytes"));
            }
            u32::try_from(text.len()).map_err(|_| Error::Limit("normalized bytes"))?;
            // ASCII folding preserves UAX #29 classes and all token byte lengths.
            return match_words(text, limits, max_steps, |word| {
                word.eq_ignore_ascii_case(self.term)
            });
        }
        let mut budget = MemoryBudget::new(limits.memory_bytes);
        let (normalized, _) = profile_text(text, limits.normalized_bytes, &mut budget)?;
        u32::try_from(normalized.len()).map_err(|_| Error::Limit("normalized bytes"))?;
        match_words(&normalized, limits, max_steps, |word| word == self.term)
    }
}

/// borrows one phrase and keeps at most 64 document-token references on the stack.
/// longer phrases and compound expressions retain the document oracle.
#[derive(Clone, Copy, Debug)]
pub struct PhraseMatcher<'q> {
    terms: &'q [String],
}

impl<'q> PhraseMatcher<'q> {
    /// accepts a single, nonempty phrase with bounded window scratch.
    pub fn new(query: &'q Query) -> Option<Self> {
        if query.node_count() != 1 {
            return None;
        }
        match &query.nodes[query.root].kind {
            Kind::Phrase(terms) if !terms.is_empty() && terms.len() <= 64 => Some(Self { terms }),
            _ => None,
        }
    }

    /// checks consecutive token windows without retaining a document token vector.
    /// ASCII borrows its input; Unicode uses the existing budgeted normalizer.
    /// The stack window contains references only, and none escape this call.
    ///
    /// # Errors
    /// preserves input, normalized-byte, term and token limits, including invalid
    /// tails after a hit. Search work matches the single-phrase oracle, including
    /// incomplete final windows and analysis errors preceding search-work errors.
    /// Unicode allocation and memory-budget failures propagate without a result.
    pub fn matches(&self, text: &str, limits: AnalysisLimits, max_steps: usize) -> Result<bool> {
        check_input(text, limits)?;
        if text.is_ascii() {
            if text.len() > limits.normalized_bytes {
                return Err(Error::Limit("normalized bytes"));
            }
            u32::try_from(text.len()).map_err(|_| Error::Limit("normalized bytes"))?;
            return match_phrase_words(text, self.terms, limits, max_steps, |word, term| {
                word.eq_ignore_ascii_case(term)
            });
        }
        let mut budget = MemoryBudget::new(limits.memory_bytes);
        let (normalized, _) = profile_text(text, limits.normalized_bytes, &mut budget)?;
        u32::try_from(normalized.len()).map_err(|_| Error::Limit("normalized bytes"))?;
        match_phrase_words(&normalized, self.terms, limits, max_steps, |word, term| {
            word == term
        })
    }
}

fn match_phrase_words(
    text: &str,
    terms: &[String],
    limits: AnalysisLimits,
    max_steps: usize,
    mut equal: impl FnMut(&str, &str) -> bool,
) -> Result<bool> {
    let mut window = [""; 64];
    let width = terms.len();
    let mut cursor = 0;
    let mut filled = 0;
    let mut tokens = 0u32;
    let mut found = false;
    let mut work = Work::new(max_steps);
    let mut exhausted = work.charge(1).err();
    for word in text.unicode_words() {
        if word.len() > limits.term_bytes {
            return Err(Error::Limit("term bytes"));
        }
        if tokens == limits.tokens {
            return Err(Error::Limit("element count"));
        }
        tokens += 1;
        // a hit or exhausted work skips comparisons, never validation of the tail.
        if found || exhausted.is_some() {
            continue;
        }
        window[cursor] = word;
        cursor += 1;
        if cursor == width {
            cursor = 0;
        }
        filled = (filled + 1).min(width);
        if filled != width {
            continue;
        }
        let mut matches = true;
        for (word, term) in window[cursor..width]
            .iter()
            .chain(&window[..cursor])
            .zip(terms)
        {
            if let Err(error) = work.charge(1) {
                exhausted = Some(error);
                matches = false;
                break;
            }
            if !equal(word, term) {
                matches = false;
                break;
            }
        }
        found = matches;
    }
    match exhausted {
        Some(error) => Err(error),
        None => Ok(found),
    }
}

fn match_words(
    text: &str,
    limits: AnalysisLimits,
    max_steps: usize,
    mut equal: impl FnMut(&str) -> bool,
) -> Result<bool> {
    let mut tokens = 0u32;
    let mut first_match = None;
    let mut comparisons_left = max_steps.saturating_sub(1);
    for word in text.unicode_words() {
        if word.len() > limits.term_bytes {
            return Err(Error::Limit("term bytes"));
        }
        if tokens == limits.tokens {
            return Err(Error::Limit("element count"));
        }
        tokens += 1;
        if first_match.is_none() && comparisons_left != 0 {
            comparisons_left -= 1;
            if equal(word) {
                first_match = Some(tokens);
            }
        }
    }
    // do not let an early match or work error hide an invalid document tail.
    let comparisons =
        usize::try_from(first_match.unwrap_or(tokens)).map_err(|_| Error::Limit("search work"))?;
    let mut work = Work::new(max_steps);
    work.charge(1)?;
    work.charge(comparisons)?;
    Ok(first_match.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_work_stops_comparisons_but_still_validates_the_tail() {
        for steps in 0..=5 {
            let mut comparisons = 0;
            let result = match_words("a b c d", AnalysisLimits::default(), steps, |_| {
                comparisons += 1;
                false
            });
            assert_eq!(comparisons, steps.saturating_sub(1).min(4));
            assert_eq!(
                result,
                if steps < 5 {
                    Err(Error::Limit("search work"))
                } else {
                    Ok(false)
                }
            );
        }
        let limits = AnalysisLimits {
            term_bytes: 1,
            ..AnalysisLimits::default()
        };
        let result = match_words("a invalid_tail", limits, 0, |_| {
            panic!("zero comparison budget must not call equality")
        });
        assert_eq!(result, Err(Error::Limit("term bytes")));
    }
}
