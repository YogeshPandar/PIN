// document-at-a-time semantic oracle, independent of dictionaries and postings.
// phrase windows check distinct occurrences; null remains outside this bool api.

use crate::analysis::Analyzed;
use crate::budget::MemoryBudget;
use crate::error::{Error, Result};
use crate::memory::{Work, vector};
use crate::query::{Kind, Query};

pub fn matches(document: &Analyzed, query: &Query, memory_bytes: usize, max_steps: usize) -> Result<bool> {
    if document.profile() != query.profile() { return Err(Error::InvalidProfile); }
    let mut budget = MemoryBudget::new(memory_bytes);
    let mut work = Work::new(max_steps);
    let mut values: Vec<bool> = vector(query.node_count(), &mut budget)?;
    for node in &query.nodes {
        work.charge(1)?;
        let value = match &node.kind {
            Kind::None => false,
            Kind::Term(term) => {
                let mut found = false;
                for token in document.tokens() {
                    work.charge(1)?;
                    if token.term == term { found = true; break; }
                }
                found
            }
            Kind::Prefix(prefix) => {
                let mut found = false;
                for token in document.tokens() {
                    work.charge(1)?;
                    if token.term.starts_with(prefix) { found = true; break; }
                }
                found
            }
            Kind::Phrase(terms) => {
                let mut found = false;
                for start in 0..document.len() as usize {
                    if terms.len() > document.len() as usize - start { break; }
                    let mut equal = true;
                    for (distance, term) in terms.iter().enumerate() {
                        work.charge(1)?;
                        if document.token(start + distance).is_none_or(|token| token.term != term) {
                            equal = false; break;
                        }
                    }
                    if equal { found = true; break; }
                }
                found
            }
            Kind::Not(child) => !values[*child],
            Kind::And(left, right) => values[*left] && values[*right],
            Kind::Or(left, right) => values[*left] || values[*right],
        };
        values.push(value);
    }
    Ok(values[query.root])
}

pub fn matches_nullable(document: Option<&Analyzed>, query: &Query, memory_bytes: usize, max_steps: usize) -> Result<Option<bool>> {
    document.map(|document| matches(document, query, memory_bytes, max_steps)).transpose()
}
