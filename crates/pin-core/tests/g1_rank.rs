use pin_core::analysis::{AnalysisLimits, Analyzed, PROFILE_ID};
use pin_core::codec::records::{DocumentRecord, Publication};
use pin_core::error::Error;
use pin_core::identity::{Generation, HeapLayout, Incarnation, RelationGeneration, RootTid, SegmentId};
use pin_core::index::{Document, IndexLimits, ReferenceIndex, SearchLimits};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use pin_core::rank::{Bm25, CorpusSummary, RankRequest, StatisticsLimits, StatsEpoch, TermStatistic};

fn relation() -> RelationGeneration { RelationGeneration::new(1, 2, 3, Generation::new(1).unwrap()).unwrap() }
fn analyzed(text: &str) -> Analyzed { Analyzed::analyze(text, AnalysisLimits::default()).unwrap() }
fn query(text: &str) -> Query { Query::parse(text, QueryLimits::default()).unwrap() }
fn documents(texts: &[Analyzed]) -> Vec<Document<'_>> {
    texts.iter().enumerate().map(|(i, text)| Document::new(relation(), DocumentRecord {
        segment: SegmentId::new(1).unwrap(), incarnation: Incarnation::new(i as u64 + 1).unwrap(),
        root: RootTid::new(i as u32, 1, HeapLayout::new(128).unwrap()).unwrap(),
        token_count: text.len(), profile: PROFILE_ID, publication: Publication::Published, live: true,
    }, text).unwrap()).collect()
}
fn summary(n: u64, length: u64) -> CorpusSummary { CorpusSummary { relation: relation(), profile: PROFILE_ID, documents: n, total_length: length } }

#[test]
fn exhaustive_scores_and_topk_match_independent_formula_and_sort() {
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
    let terms = ["a", "b", "c", "delta"];
    let mut texts = Vec::new();
    for _ in 0..127 {
        let mut text = String::new();
        for _ in 0..next() % 32 { text.push_str(terms[next() as usize % terms.len()]); text.push(' '); }
        texts.push(analyzed(&text));
    }
    let docs = documents(&texts);
    let index = ReferenceIndex::build(relation(), &docs, IndexLimits::default()).unwrap();
    let epoch = index.statistics(1).unwrap();
    let query = query("a OR b OR c OR delta");
    let average = texts.iter().map(|text| text.len() as f64).sum::<f64>() / texts.len() as f64;
    let mut expected = Vec::new();
    for (ordinal, doc) in docs.iter().enumerate() {
        if ordinal % 3 == 0 || !oracle::matches(doc.analyzed(), &query, 1 << 20, 1_000_000).unwrap() { continue; }
        let mut score = 0.0;
        for term in terms {
            let df = texts.iter().filter(|text| text.tokens().any(|token| token.term == term)).count() as f64;
            let tf = doc.analyzed().tokens().filter(|token| token.term == term).count() as f64;
            if tf != 0.0 {
                let idf = (1.0 + (texts.len() as f64 - df + 0.5) / (df + 0.5)).ln();
                score += idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * f64::from(doc.length()) / average));
            }
        }
        expected.push((ordinal, score));
    }
    expected.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    for k in [0, 1, 2, 10, 40, 127, usize::MAX] {
        for offset in [0, 3, 19, 127] {
            if k.checked_add(offset).is_none() { continue; }
            let request = RankRequest { limit: k, offset, ..RankRequest::default() };
            let actual = index.rank(&query, epoch, request, |id| Ok(id.root.block() % 3 != 0)).unwrap();
            let expected: Vec<_> = expected.iter().skip(offset).take(k).collect();
            assert_eq!(actual.len(), expected.len());
            for (actual, &(ordinal, score)) in actual.iter().zip(expected) {
                assert_eq!(actual.document, docs[ordinal].identity());
                assert!((actual.score - score).abs() < 1e-12, "{} != {score}", actual.score);
            }
        }
    }
}

#[test]
fn eligibility_precedes_heap_admission_and_ties_are_root_ordered() {
    let texts = [analyzed("a a a a"), analyzed("a"), analyzed("a b b b"), analyzed("")];
    let docs = documents(&texts);
    let index = ReferenceIndex::build(relation(), &docs, IndexLimits::default()).unwrap();
    let epoch = index.statistics(7).unwrap();
    let result = index.rank(&query("a"), epoch, RankRequest { limit: 1, ..RankRequest::default() }, |id| Ok(id.root.block() != 0)).unwrap();
    assert_eq!(result[0].document, docs[1].identity());
    let result = index.rank(&query("NOT missing"), epoch, RankRequest { limit: 2, ..RankRequest::default() }, |_| Ok(true)).unwrap();
    assert_eq!(result.iter().map(|hit| hit.document.root.block()).collect::<Vec<_>>(), [0, 1]);
    assert!(result.iter().all(|hit| hit.score == 0.0));
    assert!(matches!(index.rank(&query("a"), epoch, RankRequest::default(), |_| Err(Error::InvalidDocument)), Err(Error::InvalidDocument)));
    let mut calls = 0;
    assert!(index.rank(&query("a"), epoch, RankRequest { limit: 0, ..RankRequest::default() }, |_| { calls += 1; Ok(true) }).unwrap().is_empty());
    assert_eq!(calls, 0);
}

#[test]
fn duplicate_terms_phrases_prefixes_and_negation_have_explicit_scoring() {
    let texts = [analyzed("a a"), analyzed("a b"), analyzed("alpha alpine"), analyzed("b")];
    let docs = documents(&texts);
    let index = ReferenceIndex::build(relation(), &docs, IndexLimits::default()).unwrap();
    let epoch = index.statistics(1).unwrap();
    let rank = |source| index.rank(&query(source), epoch, RankRequest::default(), |_| Ok(true)).unwrap();
    assert_eq!(rank("a"), rank("a OR a"));
    assert_eq!(rank("a"), rank("NOT NOT a"));
    assert_eq!(rank("a"), rank("a OR NOT NOT a"));
    assert_eq!(rank("alp*"), rank("alpha OR alpine"));
    assert_eq!(rank("\"a a\"")[0].score, rank("a").iter().find(|hit| hit.document == docs[0].identity()).unwrap().score);
    assert!(rank("NOT a").iter().all(|hit| hit.score == 0.0));
    let a_score = rank("a").iter().find(|hit| hit.document == docs[0].identity()).unwrap().score;
    assert_eq!(rank("a AND NOT b")[0].score, a_score);
}

#[test]
fn epoch_coherence_foreign_generations_and_budget_excess_are_rejected() {
    let invalid = [
        vec![TermStatistic { term: "a", documents: 3 }],
        vec![TermStatistic { term: "a", documents: 2 }, TermStatistic { term: "b", documents: 2 }],
        vec![TermStatistic { term: "b", documents: 1 }, TermStatistic { term: "a", documents: 1 }],
        vec![TermStatistic { term: "a", documents: 1 }, TermStatistic { term: "a", documents: 1 }],
        vec![TermStatistic { term: "", documents: 0 }],
    ];
    for terms in &invalid { assert!(StatsEpoch::new(1, summary(2, 3), terms, StatisticsLimits::default()).is_err()); }
    assert!(StatsEpoch::new(0, summary(0, 0), &[], StatisticsLimits::default()).is_err());
    assert!(StatsEpoch::new(1, summary(0, 1), &[], StatisticsLimits::default()).is_err());
    assert!(StatsEpoch::new(1, summary(1, u64::MAX), &[], StatisticsLimits::default()).is_err());
    assert!(StatsEpoch::new(1, CorpusSummary { profile: 2, ..summary(0, 0) }, &[], StatisticsLimits::default()).is_err());
    let texts = [analyzed("a")]; let docs = documents(&texts);
    let index = ReferenceIndex::build(relation(), &docs, IndexLimits::default()).unwrap();
    let epoch = StatsEpoch::new(1, summary(0, 0), &[], StatisticsLimits::default()).unwrap();
    assert_eq!(epoch.summary().average_length(), 1.0);
    assert_eq!(epoch.document_frequency("a"), 0);
    assert!((index.rank(&query("a"), epoch, RankRequest::default(), |_| Ok(true)).unwrap()[0].score - std::f64::consts::LN_2).abs() < 1e-12);
    let foreign = RelationGeneration::new(1, 2, 4, Generation::new(1).unwrap()).unwrap();
    let wrong = StatsEpoch::new(1, CorpusSummary { relation: foreign, ..summary(0, 0) }, &[], StatisticsLimits::default()).unwrap();
    assert!(matches!(index.rank(&query("a"), wrong, RankRequest::default(), |_| Ok(true)), Err(Error::ForeignRelation)));
    assert!(index.rank(&query("a"), epoch, RankRequest { offset: 1, limit: usize::MAX, ..RankRequest::default() }, |_| Ok(true)).is_err());
    assert!(index.rank(&query("a"), epoch, RankRequest { search: SearchLimits { memory_bytes: 0, ..SearchLimits::default() }, ..RankRequest::default() }, |_| Ok(true)).is_err());
}

#[test]
fn finite_kernel_inputs_and_extreme_arithmetic_fail_closed() {
    for (k1, b) in [(0.0, 0.5), (-1.0, 0.5), (f64::NAN, 0.5), (f64::INFINITY, 0.5), (1.2, -0.1), (1.2, 1.1), (1.2, f64::NAN)] {
        assert!(Bm25::new(k1, b).is_err());
    }
    let bm25 = Bm25::default();
    for boost in [f64::NAN, f64::INFINITY, -1.0] { assert!(bm25.term_score(summary(1, 1), 1, 1, 1, boost).is_err()); }
    assert_eq!(bm25.term_score(summary(0, 0), 0, 0, 0, 1.0).unwrap(), 0.0);
    assert_eq!(bm25.term_score(summary(10, 0), 0, 0, 0, 1.0).unwrap(), 0.0);
    assert_eq!(bm25.term_score(summary(1, 1), 1, 1, 1, 0.0).unwrap(), 0.0);
    assert!(bm25.term_score(summary(1, 1), 2, 1, 1, 1.0).is_err());
    assert!(bm25.term_score(summary(1, 1), 1, 2, 1, 1.0).is_err());
    assert!(Bm25::new(f64::MAX, 1.0).unwrap().term_score(summary(1, 2), 1, 2, 2, 1.0).is_err());
    assert!(bm25.term_score(summary(u64::MAX, u64::MAX), u64::MAX, u32::MAX, u32::MAX, 1.0).unwrap().is_finite());
}
