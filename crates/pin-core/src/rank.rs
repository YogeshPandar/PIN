// exhaustive bm25 over one immutable physical-corpus epoch, without pruning.
// only caller-approved candidates enter the heap; ties use ascending root tids.
// contracts: docs/g1-semantics.md and docs/g1-api-evidence.md.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::num::NonZeroU64;

use crate::analysis::PROFILE_ID;
use crate::error::{Error, Result};
use crate::identity::{DocumentRef, RelationGeneration};
use crate::index::{Bound, ReferenceIndex, Search, SearchLimits, Term};
use crate::memory::vector;
use crate::query::Query;

#[derive(Clone, Copy, Debug)]
pub struct CorpusSummary {
    pub relation: RelationGeneration,
    pub profile: u32,
    pub documents: u64,
    pub total_length: u64,
}

impl CorpusSummary {
    fn validate(self) -> Result<()> {
        if self.profile != PROFILE_ID { return Err(Error::InvalidProfile); }
        if self.documents == 0 && self.total_length != 0 { return Err(Error::InvalidStatistics); }
        if self.documents.checked_mul(u64::from(u32::MAX)).is_some_and(|max| self.total_length > max) {
            return Err(Error::InvalidStatistics);
        }
        Ok(())
    }

    pub fn average_length(self) -> f64 {
        if self.documents == 0 || self.total_length == 0 { 1.0 }
        else { self.total_length as f64 / self.documents as f64 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TermStatistic<'a> {
    pub term: &'a str,
    pub documents: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct StatisticsLimits {
    pub terms: usize,
    pub term_bytes: usize,
    pub total_term_bytes: usize,
}

impl Default for StatisticsLimits {
    fn default() -> Self { Self { terms: 1_000_000, term_bytes: 1024, total_term_bytes: 64 << 20 } }
}

#[derive(Clone, Copy)]
enum Statistics<'a> {
    Supplied(&'a [TermStatistic<'a>]),
    Indexed(&'a [Term<'a>]),
}

#[derive(Clone, Copy)]
pub struct StatsEpoch<'a> {
    id: NonZeroU64,
    summary: CorpusSummary,
    terms: Statistics<'a>,
}

impl<'a> StatsEpoch<'a> {
    // borrows immutable sorted statistics; validates coherence without allocation.
    pub fn new(id: u64, summary: CorpusSummary, terms: &'a [TermStatistic<'a>], limits: StatisticsLimits) -> Result<Self> {
        let id = NonZeroU64::new(id).ok_or(Error::InvalidStatistics)?;
        summary.validate()?;
        if terms.len() > limits.terms { return Err(Error::Limit("statistics terms")); }
        let mut bytes = 0usize;
        let mut sum = 0u64;
        let mut previous = "";
        for term in terms {
            bytes = bytes.checked_add(term.term.len()).ok_or(Error::Limit("statistics bytes"))?;
            if term.term.len() > limits.term_bytes || bytes > limits.total_term_bytes { return Err(Error::Limit("statistics bytes")); }
            if term.term <= previous || term.documents > summary.documents { return Err(Error::InvalidStatistics); }
            sum = sum.checked_add(term.documents).ok_or(Error::InvalidStatistics)?;
            if sum > summary.total_length { return Err(Error::InvalidStatistics); }
            previous = term.term;
        }
        Ok(Self { id, summary, terms: Statistics::Supplied(terms) })
    }

    pub const fn id(self) -> u64 { self.id.get() }
    pub const fn summary(self) -> CorpusSummary { self.summary }

    pub fn document_frequency(self, term: &str) -> u64 {
        match self.terms {
            Statistics::Supplied(terms) => terms.binary_search_by(|entry| entry.term.cmp(term)).map_or(0, |index| terms[index].documents),
            Statistics::Indexed(terms) => terms.binary_search_by(|entry| entry.text.cmp(term)).map_or(0, |index| terms[index].frequency),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Bm25 {
    k1: f64,
    b: f64,
}

impl Default for Bm25 {
    fn default() -> Self { Self { k1: 1.2, b: 0.75 } }
}

impl Bm25 {
    pub fn new(k1: f64, b: f64) -> Result<Self> {
        if !k1.is_finite() || k1 <= 0.0 || !b.is_finite() || !(0.0..=1.0).contains(&b) {
            return Err(Error::InvalidParameters);
        }
        Ok(Self { k1, b })
    }

    pub fn term_score(self, summary: CorpusSummary, df: u64, tf: u32, length: u32, boost: f64) -> Result<f64> {
        summary.validate()?;
        self.weighted_score(idf(summary, df, boost)?, tf, length, summary.average_length())
    }

    fn weighted_score(self, weight: f64, tf: u32, length: u32, average: f64) -> Result<f64> {
        if tf > length { return Err(Error::InvalidStatistics); }
        if tf == 0 || weight == 0.0 { return Ok(0.0); }
        let tf = f64::from(tf);
        let norm = self.k1 * (1.0 - self.b + self.b * (f64::from(length) / average));
        let numerator = tf * (self.k1 + 1.0);
        let denominator = tf + norm;
        if !norm.is_finite() || !numerator.is_finite() || !denominator.is_finite() || denominator <= 0.0 {
            return Err(Error::NonFiniteScore);
        }
        finite(weight * (numerator / denominator))
    }
}

fn idf(summary: CorpusSummary, frequency: u64, boost: f64) -> Result<f64> {
    if !boost.is_finite() || boost < 0.0 { return Err(Error::InvalidParameters); }
    if frequency > summary.total_length { return Err(Error::InvalidStatistics); }
    let missing = summary.documents.checked_sub(frequency).ok_or(Error::InvalidStatistics)?;
    if boost == 0.0 { return Ok(0.0); }
    finite(((missing as f64 + 0.5) / (frequency as f64 + 0.5)).ln_1p() * boost)
}

fn finite(score: f64) -> Result<f64> {
    if !score.is_finite() || score < 0.0 { return Err(Error::NonFiniteScore); }
    Ok(if score == 0.0 { 0.0 } else { score })
}

#[derive(Clone, Copy, Debug)]
pub struct RankRequest {
    pub bm25: Bm25,
    pub limit: usize,
    pub offset: usize,
    pub search: SearchLimits,
}

impl Default for RankRequest {
    fn default() -> Self { Self { bm25: Bm25::default(), limit: 10, offset: 0, search: SearchLimits::default() } }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RankedCandidate {
    pub document: DocumentRef,
    pub score: f64,
}

struct HeapEntry {
    ordinal: u32,
    score: f64,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool { self.cmp(other) == Ordering::Equal }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // the least competitive entry stays at the max-heap root.
        other.score.total_cmp(&self.score).then_with(|| self.ordinal.cmp(&other.ordinal))
    }
}

struct Weight {
    term: usize,
    value: f64,
}

impl ReferenceIndex<'_> {
    pub fn statistics(&self, id: u64) -> Result<StatsEpoch<'_>> {
        let id = NonZeroU64::new(id).ok_or(Error::InvalidStatistics)?;
        Ok(StatsEpoch { id, summary: CorpusSummary {
            relation: self.relation(), profile: self.profile(), documents: self.documents().len() as u64, total_length: self.total_length(),
        }, terms: Statistics::Indexed(&self.terms) })
    }

    // errors discard the result; caller eligibility must include visibility and all filters.
    pub fn rank<F>(&self, query: &Query, epoch: StatsEpoch<'_>, request: RankRequest, mut eligible: F) -> Result<Vec<RankedCandidate>>
    where F: FnMut(DocumentRef) -> Result<bool> {
        if epoch.summary.relation != self.relation() { return Err(Error::ForeignRelation); }
        if epoch.summary.profile != self.profile() { return Err(Error::InvalidProfile); }
        let required = request.offset.checked_add(request.limit).ok_or(Error::Limit("top-k offset"))?;
        let mut search = self.search(query, request.search)?;
        let capacity = required.min(self.documents().len());
        if request.limit == 0 || request.offset >= capacity { return Ok(Vec::new()); }
        let weights = self.weights(&mut search, epoch)?;
        let requested = capacity.checked_mul(std::mem::size_of::<HeapEntry>()).ok_or(Error::Limit("top-k bytes"))?;
        if requested > search.budget.remaining() { return Err(Error::Limit("top-k bytes")); }
        let mut heap = BinaryHeap::<HeapEntry>::new();
        heap.try_reserve_exact(capacity).map_err(|_| Error::Allocation)?;
        search.budget.charge_array::<HeapEntry>(heap.capacity())?;
        let average = epoch.summary.average_length();
        while let Some(candidate) = search.next() {
            let candidate = candidate?;
            search.work.charge(1)?;
            if !eligible(candidate.document)? { continue; }
            let length = self.documents()[candidate.ordinal as usize].length();
            let mut score = 0.0;
            for weight in &weights {
                search.work.charge(1)?;
                let positions = self.document_postings(weight.term, candidate.ordinal, &mut search.work)?;
                let frequency = u32::try_from(positions.len()).map_err(|_| Error::InvalidStatistics)?;
                score = finite(score + request.bm25.weighted_score(weight.value, frequency, length, average)?)?;
            }
            let entry = HeapEntry { ordinal: candidate.ordinal, score };
            if heap.len() < capacity { heap.push(entry); }
            else if let Some(mut worst) = heap.peek_mut() {
                if entry < *worst { *worst = entry; }
            }
        }
        let entries = heap.into_sorted_vec();
        let count = entries.len().saturating_sub(request.offset).min(request.limit);
        let mut output = vector(count, &mut search.budget)?;
        for entry in entries.into_iter().skip(request.offset).take(request.limit) {
            output.push(RankedCandidate { document: self.documents()[entry.ordinal as usize].identity(), score: entry.score });
        }
        Ok(output)
    }

    fn weights(&self, search: &mut Search<'_, '_>, epoch: StatsEpoch<'_>) -> Result<Vec<Weight>> {
        let mut count = 0usize;
        for node in &search.nodes {
            let additional = match node {
                Bound::Term(_) => 1,
                Bound::Prefix(range) | Bound::Phrase(range) => range.len(),
                _ => 0,
            };
            count = count.checked_add(additional).ok_or(Error::Limit("scoring terms"))?;
        }
        let mut weights = vector(count, &mut search.budget)?;
        let mut negative = vector(search.nodes.len(), &mut search.budget)?;
        negative.resize(search.nodes.len(), false);
        for (slot, node) in search.nodes.iter().enumerate().rev() {
            search.work.charge(1)?;
            let polarity = negative[slot];
            match node {
                Bound::Not(child) => negative[*child] = !polarity,
                Bound::And(left, right) | Bound::Or(left, right) => { negative[*left] = polarity; negative[*right] = polarity; }
                Bound::Term(Some(term)) if !polarity => weights.push(Weight { term: *term, value: 0.0 }),
                Bound::Phrase(range) if !polarity => {
                    for token in &search.phrase_terms[range.clone()] {
                        search.work.charge(1)?;
                        if let Some(term) = token.term { weights.push(Weight { term, value: 0.0 }); }
                    }
                }
                Bound::Prefix(range) if !polarity => {
                    for term in range.clone() { search.work.charge(1)?; weights.push(Weight { term, value: 0.0 }); }
                }
                _ => {}
            }
        }
        let scratch = negative.capacity() * std::mem::size_of::<bool>();
        drop(negative);
        search.budget.release(scratch)?;
        weights.sort_unstable_by_key(|weight| weight.term);
        weights.dedup_by_key(|weight| weight.term);
        for weight in &mut weights {
            search.work.charge(1)?;
            weight.value = idf(epoch.summary, epoch.document_frequency(self.terms[weight.term].text), 1.0)?;
        }
        Ok(weights)
    }
}
