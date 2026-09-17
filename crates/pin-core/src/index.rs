// immutable reference postings borrow analyzed text and own only index metadata.
// one complete live incarnation per root; candidates never certify visibility.
// contracts and complexity: docs/g1-semantics.md and docs/g1-api-evidence.md.

use std::iter::FusedIterator;
use std::ops::Range;

use crate::analysis::{Analyzed, PROFILE_ID};
use crate::budget::MemoryBudget;
use crate::codec::records::{DocumentRecord, Publication};
use crate::error::{Error, Result};
use crate::identity::{DocumentRef, RelationGeneration};
use crate::memory::{Work, vector};
use crate::query::{Kind, Query};

#[derive(Clone, Copy, Debug)]
pub struct Document<'a> {
    identity: DocumentRef,
    text: &'a Analyzed,
}

impl<'a> Document<'a> {
    // rejects incomplete, removed, mismatched-profile or mismatched-length records.
    pub fn new(relation: RelationGeneration, record: DocumentRecord, text: &'a Analyzed) -> Result<Self> {
        if record.publication != Publication::Published || !record.live {
            return Err(Error::InvalidState);
        }
        if record.profile != text.profile() {
            return Err(Error::InvalidProfile);
        }
        if record.token_count != text.len() {
            return Err(Error::InvalidDocument);
        }
        Ok(Self {
            identity: DocumentRef { relation, segment: record.segment, incarnation: record.incarnation, root: record.root },
            text,
        })
    }

    pub const fn identity(self) -> DocumentRef { self.identity }
    pub const fn analyzed(self) -> &'a Analyzed { self.text }
    pub fn length(self) -> u32 { self.text.len() }
}

#[derive(Clone, Copy, Debug)]
pub struct IndexLimits {
    pub documents: usize,
    pub tokens: usize,
    pub terms: usize,
    pub term_bytes: usize,
    pub memory_bytes: usize,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self { documents: 65_536, tokens: 4_000_000, terms: 1_000_000, term_bytes: 1024, memory_bytes: 128 << 20 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SearchLimits {
    pub expanded_terms: usize,
    pub memory_bytes: usize,
    pub work_steps: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self { expanded_terms: 4096, memory_bytes: 8 << 20, work_steps: 100_000_000 }
    }
}

struct Posting<'a> {
    term: &'a str,
    document: u32,
    position: u32,
}

pub(crate) struct Term<'a> {
    pub(crate) text: &'a str,
    postings: Range<usize>,
    pub(crate) frequency: u64,
}

pub struct ReferenceIndex<'a> {
    relation: RelationGeneration,
    documents: Vec<Document<'a>>,
    postings: Vec<Posting<'a>>,
    pub(crate) terms: Vec<Term<'a>>,
    total_length: u64,
    retained_bytes: usize,
}

impl<'a> ReferenceIndex<'a> {
    // borrows input text; rejects duplicate roots, foreign generations and budget excess.
    pub fn build(relation: RelationGeneration, documents: &[Document<'a>], limits: IndexLimits) -> Result<Self> {
        if documents.len() > limits.documents || u32::try_from(documents.len()).is_err() {
            return Err(Error::Limit("documents"));
        }
        let mut total = 0usize;
        for document in documents {
            if document.identity.relation != relation { return Err(Error::ForeignRelation); }
            total = total.checked_add(document.length() as usize).ok_or(Error::Limit("tokens"))?;
            if total > limits.tokens { return Err(Error::Limit("tokens")); }
            for token in document.text.tokens() {
                if token.term.len() > limits.term_bytes { return Err(Error::Limit("term bytes")); }
            }
        }
        let mut budget = MemoryBudget::new(limits.memory_bytes);
        let mut ordered = vector(documents.len(), &mut budget)?;
        ordered.extend_from_slice(documents);
        ordered.sort_unstable_by_key(|document| document.identity.root);
        if ordered.windows(2).any(|pair| pair[0].identity.root == pair[1].identity.root) {
            return Err(Error::DuplicateDocument);
        }
        let mut postings = vector(total, &mut budget)?;
        for (ordinal, document) in ordered.iter().enumerate() {
            for token in document.text.tokens() {
                postings.push(Posting { term: token.term, document: ordinal as u32, position: token.position });
            }
        }
        postings.sort_unstable_by(|left, right| {
            (left.term, left.document, left.position).cmp(&(right.term, right.document, right.position))
        });
        let distinct = usize::from(!postings.is_empty())
            + postings.windows(2).filter(|pair| pair[0].term != pair[1].term).count();
        if distinct > limits.terms { return Err(Error::Limit("dictionary terms")); }
        let mut terms = vector(distinct, &mut budget)?;
        let mut begin = 0;
        while begin < postings.len() {
            let text = postings[begin].term;
            let mut end = begin + 1;
            let mut frequency = 1;
            while end < postings.len() && postings[end].term == text {
                if postings[end - 1].document != postings[end].document { frequency += 1; }
                end += 1;
            }
            terms.push(Term { text, postings: begin..end, frequency });
            begin = end;
        }
        Ok(Self {
            relation, documents: ordered, postings, terms,
            total_length: u64::try_from(total).map_err(|_| Error::Limit("total length"))?,
            retained_bytes: budget.used(),
        })
    }

    pub const fn relation(&self) -> RelationGeneration { self.relation }
    pub const fn profile(&self) -> u32 { PROFILE_ID }
    pub fn documents(&self) -> &[Document<'a>] { &self.documents }
    pub const fn total_length(&self) -> u64 { self.total_length }
    pub const fn retained_bytes(&self) -> usize { self.retained_bytes }

    pub fn document_frequency(&self, term: &str) -> u64 {
        self.terms.binary_search_by(|entry| entry.text.cmp(term)).map_or(0, |index| self.terms[index].frequency)
    }

    fn lookup(&self, term: &str, work: &mut Work) -> Result<Option<usize>> {
        let begin = lower_bound(&self.terms, work, |entry| entry.text < term)?;
        Ok(self.terms.get(begin).filter(|entry| entry.text == term).map(|_| begin))
    }

    fn prefix(&self, prefix: &str, work: &mut Work) -> Result<Range<usize>> {
        let begin = lower_bound(&self.terms, work, |entry| entry.text < prefix)?;
        let count = lower_bound(&self.terms[begin..], work, |entry| entry.text.starts_with(prefix))?;
        Ok(begin..begin + count)
    }

    pub(crate) fn document_postings(&self, term: usize, document: u32, work: &mut Work) -> Result<Range<usize>> {
        let range = self.terms[term].postings.clone();
        let postings = &self.postings[range.clone()];
        let begin = lower_bound(postings, work, |posting| posting.document < document)?;
        let count = lower_bound(&postings[begin..], work, |posting| posting.document == document)?;
        Ok(range.start + begin..range.start + begin + count)
    }

    fn has_position(&self, range: Range<usize>, position: u32, work: &mut Work) -> Result<bool> {
        let postings = &self.postings[range];
        let index = lower_bound(postings, work, |posting| posting.position < position)?;
        Ok(postings.get(index).is_some_and(|posting| posting.position == position))
    }

    // compiles once; all documents share bounded boolean and phrase scratch.
    pub fn search(&self, query: &Query, limits: SearchLimits) -> Result<Search<'_, 'a>> {
        let mut budget = MemoryBudget::new(limits.memory_bytes);
        let mut work = Work::new(limits.work_steps);
        let mut nodes = vector(query.node_count(), &mut budget)?;
        let phrase_count = query.nodes.iter().try_fold(0usize, |count, node| {
            let additional = if let Kind::Phrase(terms) = &node.kind { terms.len() } else { 0 };
            count.checked_add(additional).ok_or(Error::Limit("phrase terms"))
        })?;
        let mut phrase_terms = vector(phrase_count, &mut budget)?;
        let mut longest = 0;
        let mut expanded = 0usize;
        for node in &query.nodes {
            work.charge(1)?;
            let bound = match &node.kind {
                Kind::None => Bound::Never,
                Kind::Term(term) => Bound::Term(self.lookup(term, &mut work)?),
                Kind::Prefix(prefix) => {
                    let range = self.prefix(prefix, &mut work)?;
                    expanded = expanded.checked_add(range.len()).ok_or(Error::Limit("prefix expansion"))?;
                    if expanded > limits.expanded_terms { return Err(Error::Limit("prefix expansion")); }
                    Bound::Prefix(range)
                }
                Kind::Phrase(terms) => {
                    let begin = phrase_terms.len();
                    for (offset, term) in terms.iter().enumerate() {
                        work.charge(1)?;
                        phrase_terms.push(PhraseTerm {
                            term: self.lookup(term, &mut work)?,
                            offset: u32::try_from(offset).map_err(|_| Error::Limit("phrase positions"))?,
                        });
                    }
                    longest = longest.max(terms.len());
                    Bound::Phrase(begin..phrase_terms.len())
                }
                Kind::Not(child) => Bound::Not(*child),
                Kind::And(left, right) => Bound::And(*left, *right),
                Kind::Or(left, right) => Bound::Or(*left, *right),
            };
            nodes.push(bound);
        }
        let mut values = vector(query.node_count(), &mut budget)?;
        values.resize(query.node_count(), false);
        let mut ranges = vector(longest, &mut budget)?;
        ranges.resize(longest, 0..0);
        Ok(Search { index: self, nodes, phrase_terms, values, ranges, root: query.root,
            next_document: 0, failed: false, budget, work })
    }

    pub fn candidate_count(&self, query: &Query, limits: SearchLimits) -> Result<u64> {
        let mut count = 0;
        for candidate in self.search(query, limits)? {
            candidate?;
            count += 1;
        }
        Ok(count)
    }
}

fn lower_bound<T, F>(values: &[T], work: &mut Work, mut before: F) -> Result<usize>
where F: FnMut(&T) -> bool {
    let mut low = 0;
    let mut high = values.len();
    while low < high {
        work.charge(1)?;
        let middle = low + (high - low) / 2;
        if before(&values[middle]) { low = middle + 1; } else { high = middle; }
    }
    Ok(low)
}

pub(crate) enum Bound {
    Never,
    Term(Option<usize>),
    Prefix(Range<usize>),
    Phrase(Range<usize>),
    Not(usize),
    And(usize, usize),
    Or(usize, usize),
}

pub(crate) struct PhraseTerm {
    pub(crate) term: Option<usize>,
    offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub document: DocumentRef,
    pub(crate) ordinal: u32,
}

pub struct Search<'i, 'a> {
    index: &'i ReferenceIndex<'a>,
    pub(crate) nodes: Vec<Bound>,
    pub(crate) phrase_terms: Vec<PhraseTerm>,
    values: Vec<bool>,
    ranges: Vec<Range<usize>>,
    root: usize,
    next_document: usize,
    failed: bool,
    pub(crate) budget: MemoryBudget,
    pub(crate) work: Work,
}

impl Search<'_, '_> {
    fn matches(&mut self, document: u32) -> Result<bool> {
        for (slot, node) in self.nodes.iter().enumerate() {
            self.work.charge(1)?;
            self.values[slot] = match node {
                Bound::Never | Bound::Term(None) => false,
                Bound::Term(Some(term)) => !self.index.document_postings(*term, document, &mut self.work)?.is_empty(),
                Bound::Prefix(range) => {
                    let mut found = false;
                    for term in range.clone() {
                        self.work.charge(1)?;
                        if !self.index.document_postings(term, document, &mut self.work)?.is_empty() { found = true; break; }
                    }
                    found
                }
                Bound::Phrase(range) => phrase(self.index, &self.phrase_terms[range.clone()], &mut self.ranges, document, &mut self.work)?,
                Bound::Not(child) => !self.values[*child],
                Bound::And(left, right) => self.values[*left] && self.values[*right],
                Bound::Or(left, right) => self.values[*left] || self.values[*right],
            };
        }
        Ok(self.values[self.root])
    }

    pub fn retained_bytes(&self) -> usize { self.budget.used() }
}

impl Iterator for Search<'_, '_> {
    type Item = Result<Candidate>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed { return None; }
        while self.next_document < self.index.documents.len() {
            let ordinal = self.next_document;
            self.next_document += 1;
            match self.matches(ordinal as u32) {
                Ok(false) => continue,
                Ok(true) => return Some(Ok(Candidate { document: self.index.documents[ordinal].identity, ordinal: ordinal as u32 })),
                Err(error) => { self.failed = true; return Some(Err(error)); }
            }
        }
        None
    }
}

impl FusedIterator for Search<'_, '_> {}

fn phrase(index: &ReferenceIndex<'_>, terms: &[PhraseTerm], ranges: &mut [Range<usize>], document: u32, work: &mut Work) -> Result<bool> {
    let mut anchor = 0;
    for (slot, term) in terms.iter().enumerate() {
        work.charge(1)?;
        let Some(term) = term.term else { return Ok(false); };
        ranges[slot] = index.document_postings(term, document, work)?;
        if ranges[slot].is_empty() { return Ok(false); }
        if ranges[slot].len() < ranges[anchor].len() { anchor = slot; }
    }
    // probe positions only after every phrase term has a local posting list.
    for posting in &index.postings[ranges[anchor].clone()] {
        work.charge(1)?;
        let Some(start) = posting.position.checked_sub(terms[anchor].offset) else { continue; };
        let mut matched = true;
        for (slot, term) in terms.iter().enumerate() {
            if slot == anchor { continue; }
            work.charge(1)?;
            let Some(position) = start.checked_add(term.offset) else { matched = false; break; };
            if !index.has_position(ranges[slot].clone(), position, work)? { matched = false; break; }
        }
        if matched { return Ok(true); }
    }
    Ok(false)
}
