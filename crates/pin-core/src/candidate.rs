//! Bounded term covers for lossy bitmap scans, not exact search results.
//! Terms borrow the validated query; no document-sized result set is retained.
//! Every exact match is covered; the host must recheck all returned heap tuples.
//! Contracts: PostgreSQL 18 index-scanning and docs/g2-storage.md.

use crate::budget::MemoryBudget;
use crate::error::{Error, Result};
use crate::memory::vector;
use crate::query::{Kind, Query};

/// A union of term postings, or the complete indexed non-null universe.
#[derive(Debug, Eq, PartialEq)]
pub enum CandidatePlan<'q> {
    Empty,
    Universe,
    Terms(Vec<&'q str>),
}

impl<'q> CandidatePlan<'q> {
    /// Builds a necessary-condition cover with bounded, fallible scratch.
    ///
    /// AND selects the smaller cover; OR retains both covers. Negation and
    /// prefix matching use the universe until a dedicated exact cursor exists.
    /// The cost is a probe-count heuristic, not a selectivity estimate.
    ///
    /// # Errors
    /// Returns allocation, arithmetic, or memory-budget errors without a partial plan.
    /// The budget includes peak scratch capacity but excludes the borrowed query.
    pub fn build(query: &'q Query, memory_bytes: usize) -> Result<Self> {
        const ALL: usize = usize::MAX;
        let mut budget = MemoryBudget::new(memory_bytes);
        let mut costs: Vec<usize> = vector(query.node_count(), &mut budget)?;
        for node in &query.nodes {
            let cost = match &node.kind {
                Kind::None => 0,
                Kind::Term(_) => 1,
                Kind::Phrase(terms) => usize::from(!terms.is_empty()),
                Kind::Prefix(_) | Kind::Not(_) => ALL,
                Kind::And(left, right) => costs[*left].min(costs[*right]),
                Kind::Or(left, right) => {
                    if costs[*left] == ALL || costs[*right] == ALL {
                        ALL
                    } else {
                        costs[*left]
                            .checked_add(costs[*right])
                            .ok_or(Error::Limit("candidate covers"))?
                    }
                }
            };
            costs.push(cost);
        }
        match costs[query.root] {
            0 => return Ok(Self::Empty),
            ALL => return Ok(Self::Universe),
            _ => {}
        }
        let mut pending = vector(query.node_count(), &mut budget)?;
        let mut terms = vector(costs[query.root], &mut budget)?;
        pending.push(query.root);
        while let Some(index) = pending.pop() {
            if costs[index] == 0 {
                continue;
            }
            match &query.nodes[index].kind {
                Kind::Term(term) => terms.push(term.as_str()),
                Kind::Phrase(phrase) => {
                    if let Some(term) = phrase.first() {
                        terms.push(term.as_str());
                    }
                }
                Kind::And(left, right) => {
                    pending.push(if costs[*left] <= costs[*right] {
                        *left
                    } else {
                        *right
                    });
                }
                Kind::Or(left, right) => {
                    pending.push(*right);
                    pending.push(*left);
                }
                Kind::None | Kind::Prefix(_) | Kind::Not(_) => {
                    return Err(Error::InvalidState);
                }
            }
        }
        // sort borrowed bytes in place; repeated clauses need one posting probe.
        terms.sort_unstable();
        terms.dedup();
        Ok(Self::Terms(terms))
    }
}
