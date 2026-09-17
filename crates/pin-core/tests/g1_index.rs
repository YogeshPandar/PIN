use pin_core::analysis::{AnalysisLimits, Analyzed, PROFILE_ID};
use pin_core::codec::records::{DocumentRecord, Publication};
use pin_core::error::Error;
use pin_core::identity::{Generation, HeapLayout, Incarnation, RelationGeneration, RootTid, SegmentId};
use pin_core::index::{Document, IndexLimits, ReferenceIndex, SearchLimits};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};

fn relation() -> RelationGeneration {
    RelationGeneration::new(1, 2, 3, Generation::new(1).unwrap()).unwrap()
}

fn record(index: usize, text: &Analyzed) -> DocumentRecord {
    DocumentRecord {
        segment: SegmentId::new(1).unwrap(), incarnation: Incarnation::new(index as u64 + 1).unwrap(),
        root: RootTid::new((index / 128) as u32, (index % 128 + 1) as u16, HeapLayout::new(128).unwrap()).unwrap(),
        token_count: text.len(), profile: PROFILE_ID, publication: Publication::Published, live: true,
    }
}

fn analyze(text: &str) -> Analyzed { Analyzed::analyze(text, AnalysisLimits::default()).unwrap() }
fn query(text: &str) -> Query { Query::parse(text, QueryLimits::default()).unwrap() }

fn compare(texts: &[Analyzed], sources: &[String]) {
    let mut documents: Vec<_> = texts.iter().enumerate().map(|(i, text)| Document::new(relation(), record(i, text), text).unwrap()).collect();
    documents.reverse();
    let index = ReferenceIndex::build(relation(), &documents, IndexLimits::default()).unwrap();
    for source in sources {
        let query = query(source);
        let expected: Vec<_> = index.documents().iter().filter(|doc| oracle::matches(doc.analyzed(), &query, 1 << 20, 10_000_000).unwrap()).map(|doc| doc.identity()).collect();
        let actual: Vec<_> = index.search(&query, SearchLimits::default()).unwrap().map(|candidate| candidate.unwrap().document).collect();
        assert_eq!(actual, expected, "{source}");
        assert_eq!(index.candidate_count(&query, SearchLimits::default()).unwrap(), expected.len() as u64);
        let mut wire = [0; 4096];
        let length = query.encode(&mut wire, 4096).unwrap();
        let restored = Query::decode(&wire[..length], QueryLimits::default()).unwrap();
        assert_eq!(index.candidate_count(&restored, SearchLimits::default()).unwrap(), expected.len() as u64);
    }
}

#[test]
fn postings_match_independent_document_oracle() {
    let texts: Vec<_> = ["", "a", "a a", "a b a b", "a c b", "alpha alpine βeta", "CAFÉ cafe\u{301}", "Σ ς σ", "can't 32.3", "!?"].into_iter().map(analyze).collect();
    let queries: Vec<_> = ["", "a", "missing", "NOT missing", "NOT a", "a OR b AND NOT c", "NOT (a OR b)", "NOT NOT a", "\"a a\"", "\"b a b\"", "alp*", "z*", "CAFÉ", "\"Σ Σ\"", "NOT \"\""].into_iter().map(str::to_owned).collect();
    compare(&texts, &queries);
}

#[test]
fn seeded_corpora_cover_boolean_phrase_prefix_and_root_ordering() {
    let mut seed = 0x8128_16db_91a2_37d5u64;
    let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
    let vocabulary = ["a", "b", "c", "alpine", "alpha", "café", "βeta"];
    let mut texts = Vec::new();
    for _ in 0..193 {
        let mut text = String::new();
        for _ in 0..next() % 24 { text.push_str(vocabulary[next() as usize % vocabulary.len()]); text.push(' '); }
        texts.push(analyze(&text));
    }
    let mut queries = Vec::new();
    for _ in 0..120 {
        let a = vocabulary[next() as usize % vocabulary.len()];
        let b = vocabulary[next() as usize % vocabulary.len()];
        let c = vocabulary[next() as usize % vocabulary.len()];
        queries.push(format!("({a} OR {b}) AND NOT {c}"));
        queries.push(format!("NOT ({a} AND ({b} OR NOT {c}))"));
        queries.push(format!("\"{a} {b} {a}\" OR alp*"));
    }
    compare(&texts, &queries);
}

#[test]
fn publication_profile_and_incarnation_invariants_are_enforced() {
    let text = analyze("a b");
    let valid = record(0, &text);
    for publication in [Publication::Allocated, Publication::FragmentsWritten, Publication::Abandoned] {
        assert!(matches!(Document::new(relation(), DocumentRecord { publication, ..valid }, &text), Err(Error::InvalidState)));
    }
    assert!(Document::new(relation(), DocumentRecord { live: false, ..valid }, &text).is_err());
    assert!(Document::new(relation(), DocumentRecord { token_count: 1, ..valid }, &text).is_err());
    assert!(Document::new(relation(), DocumentRecord { profile: 2, ..valid }, &text).is_err());
    let first = Document::new(relation(), valid, &text).unwrap();
    let reused = Document::new(relation(), DocumentRecord { incarnation: Incarnation::new(2).unwrap(), ..valid }, &text).unwrap();
    assert!(matches!(ReferenceIndex::build(relation(), &[first, reused], IndexLimits::default()), Err(Error::DuplicateDocument)));
    let foreign = RelationGeneration::new(1, 2, 3, Generation::new(2).unwrap()).unwrap();
    let other = Document::new(foreign, valid, &text).unwrap();
    assert!(matches!(ReferenceIndex::build(relation(), &[other], IndexLimits::default()), Err(Error::ForeignRelation)));
}

#[test]
fn resource_errors_are_explicit_and_search_errors_are_fused() {
    let text = analyze("a alpha alpine b c");
    let documents = [Document::new(relation(), record(0, &text), &text).unwrap()];
    for limits in [IndexLimits { documents: 0, ..IndexLimits::default() }, IndexLimits { tokens: 1, ..IndexLimits::default() }, IndexLimits { terms: 1, ..IndexLimits::default() }, IndexLimits { term_bytes: 1, ..IndexLimits::default() }, IndexLimits { memory_bytes: 0, ..IndexLimits::default() }] {
        assert!(ReferenceIndex::build(relation(), &documents, limits).is_err());
    }
    let index = ReferenceIndex::build(relation(), &documents, IndexLimits::default()).unwrap();
    assert!(index.search(&query("alp*"), SearchLimits { expanded_terms: 1, ..SearchLimits::default() }).is_err());
    assert!(index.search(&query("alp* OR alp*"), SearchLimits { expanded_terms: 3, ..SearchLimits::default() }).is_err());
    assert!(index.search(&query("a"), SearchLimits { memory_bytes: 0, ..SearchLimits::default() }).is_err());
    let mut observed_error = false;
    for work in 1..100 {
        if let Ok(mut search) = index.search(&query("a"), SearchLimits { work_steps: work, ..SearchLimits::default() })
            && search.next().is_some_and(|result| result.is_err())
        {
            assert!(search.next().is_none()); assert!(search.next().is_none()); observed_error = true; break;
        }
    }
    assert!(observed_error);
    let mut search = index.search(&query("a"), SearchLimits::default()).unwrap();
    assert!(search.next().unwrap().is_ok());
    assert!(search.next().is_none()); assert!(search.next().is_none());
}

#[test]
fn repeated_phrase_terms_are_verified_without_materializing_positions() {
    let texts = [analyze(&format!("{}b a a", "a ".repeat(4096))), analyze("a b a")];
    compare(&texts, &["\"b a a\"".into(), "\"a b a\"".into(), "\"a a a a\"".into()]);
    let docs: Vec<_> = texts.iter().enumerate().map(|(i, text)| Document::new(relation(), record(i, text), text).unwrap()).collect();
    let index = ReferenceIndex::build(relation(), &docs, IndexLimits::default()).unwrap();
    assert_eq!(index.document_frequency("a"), 2);
    assert_eq!(index.document_frequency("z"), 0);
    assert_eq!(index.total_length(), texts.iter().map(|text| u64::from(text.len())).sum());
}
