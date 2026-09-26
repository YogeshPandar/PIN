use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::candidate::CandidatePlan;
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::page::{Page, PageKind};
use pin_core::mutable::{self, PageStore, document::PreparedDocument};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeSet;

#[path = "support/mutable_store.rs"]
mod mutable_store;
use mutable_store::MemoryStore;

fn root(index: u32) -> RootTid {
    RootTid::new(
        index / 200 + 1,
        (index % 200 + 1) as u16,
        HeapLayout::new(291).unwrap(),
    )
    .unwrap()
}

fn prepared(text: &str) -> PreparedDocument {
    let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
    PreparedDocument::prepare(&analyzed, 1 << 20).unwrap()
}

fn seeded(documents: &[&str]) -> MemoryStore {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    for (index, text) in documents.iter().enumerate() {
        mutable::insert(&mut store, root(index as u32), &prepared(text)).unwrap();
    }
    store
}

fn run<S: PageStore>(store: &mut S, source: &str, budget: usize) -> Result<Vec<RootTid>> {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    let count = mutable::scan_query(store, &query, budget, |root| {
        rows.push(root);
        Ok(())
    })?;
    assert_eq!(count, rows.len() as u64);
    Ok(rows)
}

#[test]
fn independent_oracle_matches_are_always_covered() {
    let documents = [
        "", "a", "b", "c", "a b", "b a", "a a", "b c", "a b c", "alphabet", "É a",
    ];
    let mut store = seeded(&documents);
    let atoms = [
        "a",
        "b",
        "missing",
        "a*",
        "\"a b\"",
        "\"a a\"",
        "NOT a",
        "NOT (a OR b)",
    ];
    let mut sources: Vec<String> = atoms.iter().map(|text| text.to_string()).collect();
    sources.push(String::new());
    for left in atoms {
        for right in atoms {
            for operator in ["AND", "OR"] {
                sources.push(format!("({left}) {operator} ({right})"));
                sources.push(format!("a OR (({left}) {operator} ({right}))"));
                sources.push(format!("NOT (({left}) {operator} ({right}))"));
            }
        }
    }
    for source in sources {
        let query = Query::parse(&source, QueryLimits::default()).unwrap();
        let mut rows = Vec::new();
        let mut proven = BTreeSet::new();
        mutable::scan_query_with_recheck(&mut store, &query, 1 << 20, |root, recheck| {
            rows.push(root);
            if !recheck {
                proven.insert(root);
            }
            Ok(())
        })
        .unwrap();
        let candidates: BTreeSet<_> = rows.iter().copied().collect();
        assert_eq!(rows.len(), candidates.len(), "duplicate owner: {source}");
        for (index, text) in documents.iter().enumerate() {
            let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
            let exact = oracle::matches(&document, &query, 1 << 20, 1 << 20).unwrap();
            assert!(
                !exact || candidates.contains(&root(index as u32)),
                "{source:?} on {text:?}"
            );
            assert!(
                !proven.contains(&root(index as u32)) || exact,
                "false proof for {source:?} on {text:?}"
            );
        }
    }
}

#[test]
fn inline_position_phrase_proofs_match_independent_text_oracle() {
    let documents = [
        "alpha beta gamma",
        "beta alpha gamma",
        "echo echo delta",
        "delta echo echo",
        "alpha, beta",
        "Éclair café",
        "alpha beta alpha beta",
    ];
    let mut store = seeded(&documents);
    for source in [
        "\"alpha beta\"",
        "\"beta gamma\"",
        "\"echo echo\"",
        "\"alpha beta alpha\"",
        "\"éclair café\"",
        "\"missing word\"",
    ] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        let mut actual = BTreeSet::new();
        mutable::scan_query_with_options(&mut store, &query, 1 << 20, true, |root, recheck| {
            assert!(!recheck, "inline phrase must be fully proven: {source}");
            assert!(actual.insert(root));
            Ok(())
        })
        .unwrap();
        let expected: BTreeSet<_> = documents
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
                oracle::matches(&document, &query, 1 << 20, 1 << 20)
                    .unwrap()
                    .then_some(root(index as u32))
            })
            .collect();
        assert_eq!(actual, expected, "{source}");
    }
}

#[test]
fn fragmented_phrase_document_keeps_heap_recheck() {
    let text = "echo ".repeat(10_000);
    let mut store = seeded(&[&text]);
    let query = Query::parse("\"echo echo\"", QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    mutable::scan_query_with_options(&mut store, &query, 1 << 20, true, |root, recheck| {
        rows.push((root, recheck));
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, vec![(root(0), true)]);
}

#[test]
fn positive_boolean_candidates_equal_the_oracle_without_shared_cursor_skips() {
    let documents = ["", "a", "b", "c", "a b", "a c", "b c", "a b c"];
    let mut store = seeded(&documents);
    for source in [
        "a AND b",
        "a OR b",
        "a OR a",
        "a AND a",
        "a OR (a AND c)",
        "(a AND c) OR a",
        "(a OR b) AND (b OR c)",
        "(a OR b) AND a",
        "(a AND b) OR (a AND c)",
        "a AND missing",
        "missing OR b",
    ] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        let rows = run(&mut store, source, 1 << 20).unwrap();
        let expected: BTreeSet<_> = documents
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
                oracle::matches(&document, &query, 1 << 20, 1 << 20)
                    .unwrap()
                    .then_some(root(index as u32))
            })
            .collect();
        assert_eq!(rows.len(), expected.len(), "{source}");
        assert_eq!(
            rows.into_iter().collect::<BTreeSet<_>>(),
            expected,
            "{source}"
        );
    }
}

#[test]
fn phrases_intersect_terms_but_do_not_claim_position_or_multiplicity_checks() {
    let mut store = seeded(&["a", "a b", "b a", "b", "a a"]);
    assert_eq!(
        run(&mut store, "\"a b\"", 1 << 20).unwrap(),
        vec![root(1), root(2)]
    );
    assert_eq!(
        run(&mut store, "\"a a\"", 1 << 20).unwrap(),
        vec![root(0), root(1), root(2), root(4)]
    );
    assert!(
        run(&mut store, "\"a missing\"", 1 << 20)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn intersections_use_owner_identity_not_reused_heap_coordinates() {
    let mut store = seeded(&["a"]);
    mutable::insert(&mut store, root(0), &prepared("b")).unwrap();
    assert!(run(&mut store, "a AND b", 1 << 20).unwrap().is_empty());
    mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
    mutable::insert(&mut store, root(0), &prepared("a b")).unwrap();
    assert_eq!(run(&mut store, "a AND b", 1 << 20).unwrap(), vec![root(0)]);
    mutable::compact(&mut store).unwrap();
    assert_eq!(run(&mut store, "a AND b", 1 << 20).unwrap(), vec![root(0)]);
}

#[test]
fn sealed_pages_mutable_tails_and_vacuum_preserve_boolean_results() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let common = prepared("a");
    let both = prepared("a b");
    for index in 0..3300 {
        mutable::insert(
            &mut store,
            root(index),
            if index % 7 == 0 { &both } else { &common },
        )
        .unwrap();
    }
    mutable::compact(&mut store).unwrap();
    for index in 3300..4000 {
        mutable::insert(
            &mut store,
            root(index),
            if index % 7 == 0 { &both } else { &common },
        )
        .unwrap();
    }
    let expected: Vec<_> = (0..4000).filter(|index| index % 7 == 0).map(root).collect();
    assert_eq!(run(&mut store, "a AND b", 1 << 20).unwrap(), expected);
    assert_eq!(run(&mut store, "a OR b", 1 << 20).unwrap().len(), 4000);
    let removed: BTreeSet<_> = (0..4000).filter(|index| index % 5 == 0).map(root).collect();
    mutable::vacuum(&mut store, |root| Ok(removed.contains(&root))).unwrap();
    mutable::compact(&mut store).unwrap();
    let expected: Vec<_> = expected
        .into_iter()
        .filter(|root| !removed.contains(root))
        .collect();
    assert_eq!(run(&mut store, "a AND b", 1 << 20).unwrap(), expected);
}

#[test]
fn cursor_budget_falls_back_before_emission_without_truncating_matches() {
    let mut store = seeded(&["a", "a b", "b"]);
    assert_eq!(run(&mut store, "a AND b", 1 << 20).unwrap(), vec![root(1)]);
    assert_eq!(
        run(&mut store, "a AND b", 256).unwrap(),
        vec![root(0), root(1)]
    );
    assert!(run(&mut store, "a AND b", 0).is_err());
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    for (budget, expected) in [
        (1 << 20, vec![(root(1), false)]),
        (256, vec![(root(0), true), (root(1), true)]),
    ] {
        let mut rows = Vec::new();
        mutable::scan_query_with_recheck(&mut store, &query, budget, |root, recheck| {
            rows.push((root, recheck));
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, expected, "fallback must discard exactness proof");
    }
    let query = Query::parse("a OR b", QueryLimits::default()).unwrap();
    let mut calls = 0;
    assert_eq!(
        mutable::scan_query(&mut store, &query, 1 << 20, |_| {
            calls += 1;
            Err(Error::InvalidParameters)
        }),
        Err(Error::InvalidParameters)
    );
    assert_eq!(
        calls, 1,
        "an output error must not replay the scan through fallback"
    );
}

#[test]
fn mutable_owner_reordering_is_rejected() {
    let mut store = seeded(&["a", "a", "a", "a"]);
    let block = (1..store.pages.len())
        .find(|&block| store.read(block as u32).unwrap().kind() == PageKind::Postings)
        .unwrap();
    let bytes = &mut store.pages[block];
    let first: [u8; 16] = bytes[24..40].try_into().unwrap();
    let second: [u8; 16] = bytes[40..56].try_into().unwrap();
    bytes[24..40].copy_from_slice(&second);
    bytes[40..56].copy_from_slice(&first);
    assert!(run(&mut store, "a", 1 << 20).is_err());
}

#[test]
fn equal_owner_coordinates_with_foreign_incarnations_are_rejected() {
    let mut store = seeded(&["a b"]);
    for block in 1..store.pages.len() {
        let page = store.read(block as u32).unwrap();
        if page.kind() != PageKind::Dictionary {
            continue;
        }
        for entry in page.terms().unwrap() {
            let entry = entry.unwrap();
            if entry.term == "b" {
                let offset = usize::from(entry.reference.offset) + 20;
                store.pages[block][offset..offset + 8].copy_from_slice(&777u64.to_le_bytes());
            }
        }
    }
    assert!(run(&mut store, "a OR b", 1 << 20).is_err());
    assert!(run(&mut store, "a AND b", 1 << 20).is_err());
}

struct CountStore {
    inner: MemoryStore,
    owner_reads: usize,
    dictionary_reads: usize,
    cancel: bool,
}

impl PageStore for CountStore {
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        let page = self.inner.read(block)?;
        self.owner_reads += usize::from(page.kind() == PageKind::Owners);
        self.dictionary_reads += usize::from(page.kind() == PageKind::Dictionary);
        Ok(page)
    }
    fn extend(&mut self) -> Result<u32> {
        self.inner.extend()
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        self.inner.commit(pages)
    }
    fn interrupt(&mut self) -> Result<()> {
        if self.cancel {
            Err(Error::InvalidParameters)
        } else {
            Ok(())
        }
    }
}

#[test]
fn filtering_avoids_owner_page_reads_for_rejected_postings() {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    let common = prepared("a");
    for index in 0..2000 {
        mutable::insert(&mut inner, root(index), &common).unwrap();
    }
    mutable::insert(&mut inner, root(2000), &prepared("a b")).unwrap();
    let mut store = CountStore {
        inner,
        owner_reads: 0,
        dictionary_reads: 0,
        cancel: false,
    };
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    let plan = CandidatePlan::build(&query, 1 << 20).unwrap();
    assert_eq!(mutable::scan(&mut store, &plan, |_| Ok(())).unwrap(), 2001);
    let baseline_reads = store.owner_reads;
    store.owner_reads = 0;
    assert_eq!(
        run(&mut store, "a AND b", 1 << 20).unwrap(),
        vec![root(2000)]
    );
    assert_eq!(store.owner_reads, 1);
    assert!(baseline_reads > store.owner_reads);
    store.owner_reads = 0;
    assert!(
        run(&mut store, "a AND missing", 1 << 20)
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.owner_reads, 0);
    store.cancel = true;
    assert_eq!(
        run(&mut store, "a AND b", 1 << 20),
        Err(Error::InvalidParameters)
    );
}

#[test]
fn repeated_terms_share_dictionary_lookup_but_keep_independent_cursors() {
    let mut store = CountStore {
        inner: seeded(&["a", "a b", "b", "a c", "c"]),
        owner_reads: 0,
        dictionary_reads: 0,
        cancel: false,
    };
    let expected = vec![root(0), root(1), root(3)];
    assert_eq!(
        run(&mut store, "(a AND b) OR a", 1 << 20).unwrap(),
        expected
    );
    assert_eq!(store.dictionary_reads, 2);

    store.dictionary_reads = 0;
    assert_eq!(
        run(&mut store, "a OR (a AND b)", 1 << 20).unwrap(),
        expected
    );
    assert_eq!(store.dictionary_reads, 2);
}

#[test]
fn deep_queries_use_bounded_explicit_continuations_not_recursive_calls() {
    let source = (0..700)
        .map(|index| if index % 2 == 0 { "a" } else { "b" })
        .collect::<Vec<_>>()
        .join(" OR ");
    let query = Query::parse(
        &source,
        QueryLimits {
            depth: 1024,
            nodes: 2048,
            terms: 1024,
            memory_bytes: 16 << 20,
            ..QueryLimits::default()
        },
    )
    .unwrap();
    let mut store = seeded(&["a", "b", "c"]);
    let mut rows = Vec::new();
    mutable::scan_query(&mut store, &query, 16 << 20, |root| {
        rows.push(root);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, vec![root(0), root(1)]);
}
