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
fn fragmented_phrase_document_uses_index_positions() {
    let text = "echo ".repeat(10_000);
    let mut store = seeded(&[&text]);
    let query = Query::parse("\"echo echo\"", QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    mutable::scan_query_with_options(&mut store, &query, 1 << 20, true, |root, recheck| {
        rows.push((root, recheck));
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, vec![(root(0), false)]);

    let mut fallback = Vec::new();
    mutable::scan_query_with_options(&mut store, &query, 16 << 10, true, |root, recheck| {
        fallback.push((root, recheck));
        Ok(())
    })
    .unwrap();
    assert_eq!(fallback, vec![(root(0), true)]);

    let absent = Query::parse("\"echo missing\"", QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    mutable::scan_query_with_options(&mut store, &absent, 1 << 20, true, |root, recheck| {
        rows.push((root, recheck));
        Ok(())
    })
    .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn fragmented_phrase_order_and_repetition_match_text_oracle() {
    let texts = [
        format!("{}alpha beta", "echo ".repeat(10_000)),
        format!("{}beta alpha", "echo ".repeat(10_000)),
        "alpha echo beta echo ".repeat(3_000),
    ];
    let mut store = seeded(&texts.iter().map(String::as_str).collect::<Vec<_>>());
    for source in [
        "\"alpha beta\"",
        "\"beta alpha\"",
        "\"alpha alpha\"",
        "\"echo echo\"",
    ] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        let expected: Vec<_> = texts
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
                oracle::matches(&analyzed, &query, 1 << 20, 1 << 20)
                    .unwrap()
                    .then_some((root(index as u32), false))
            })
            .collect();
        let mut rows = Vec::new();
        mutable::scan_query_with_options(&mut store, &query, 1 << 20, true, |root, recheck| {
            rows.push((root, recheck));
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, expected, "{source}");
    }
}

#[test]
fn fragmented_phrase_rejects_broken_chains() {
    let text = format!("{}zulu zulu", "echo ".repeat(10_000));
    let original = seeded(&[&text]);
    let query = Query::parse("\"zulu zulu\"", QueryLimits::default()).unwrap();
    let mut probe = original.clone();
    let block = (0..probe.blocks().unwrap())
        .find(|block| {
            probe
                .read(*block)
                .unwrap()
                .fragment_data()
                .is_ok_and(|(_, offset, _)| offset == 0)
        })
        .unwrap();
    let fragment = probe.read(block).unwrap();
    let (owner, offset, payload) = fragment.fragment_data().unwrap();
    for broken in [
        Page::fragment(block, owner, owner.page, offset, payload).unwrap(),
        Page::fragment(block, owner, fragment.next().unwrap(), offset + 1, payload).unwrap(),
        Page::fragment(
            block,
            owner,
            pin_core::mutable::page::NO_BLOCK,
            offset,
            payload,
        )
        .unwrap(),
    ] {
        let mut store = original.clone();
        store.pages[block as usize] = broken.bytes().to_vec();
        let mut emitted = false;
        let result = mutable::scan_query_with_options(&mut store, &query, 1 << 20, true, |_, _| {
            emitted = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!emitted);
    }
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
    fragment_reads: usize,
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
        self.fragment_reads += usize::from(page.kind() == PageKind::Fragment);
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
        fragment_reads: 0,
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
        fragment_reads: 0,
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

#[test]
fn selected_position_reader_skips_unused_deltas_but_checks_consumed_data() {
    use pin_core::mutable::document::{SelectedPositions, validate};
    let text = format!("alpha beta {}", "echo ".repeat(10_000));
    let document = prepared(&text);
    let wanted = vec!["alpha".to_string(), "beta".to_string()];
    let read = |bytes| {
        SelectedPositions::read(
            bytes,
            document.token_count(),
            document.term_count(),
            &wanted,
        )
    };
    let view = read(document.bytes()).unwrap();
    assert_eq!(view.directory_terms, 3);
    assert_eq!(view.selected_positions, 2);
    assert!(view.phrase_matches().unwrap());
    let mut corrupt = document.bytes().to_vec();
    *corrupt.last_mut().unwrap() = 0;
    // unrelated delta corruption belongs to the complete integrity validator.
    assert!(validate(&corrupt, 1 << 20).is_err());
    assert!(read(&corrupt).unwrap().phrase_matches().unwrap());
    assert!(
        SelectedPositions::read(
            &corrupt,
            document.token_count(),
            document.term_count(),
            &["echo".to_string()]
        )
        .is_err()
    );
    assert!(read(&document.bytes()[..document.bytes().len() - 1]).is_err());
    assert!(
        SelectedPositions::read(
            document.bytes(),
            document.token_count() + 1,
            document.term_count(),
            &wanted
        )
        .is_err()
    );
}

#[test]
fn phrase_witness_reads_one_fragment_without_loading_document_tail() {
    let text = format!("alpha beta {}zulu zulu", "echo ".repeat(20_000));
    let mut store = CountStore {
        inner: seeded(&[&text]),
        owner_reads: 0,
        dictionary_reads: 0,
        fragment_reads: 0,
        cancel: false,
    };
    for source in ["\"alpha beta\"", "\"echo echo\""] {
        store.fragment_reads = 0;
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        let mut rows = Vec::new();
        mutable::scan_query_with_options(&mut store, &query, 1 << 20, true, |tid, recheck| {
            rows.push((tid, recheck));
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, vec![(root(0), false)]);
        assert_eq!(store.fragment_reads, 1);
    }
    store.fragment_reads = 0;
    let query = Query::parse("\"zulu zulu\"", QueryLimits::default()).unwrap();
    assert_eq!(
        mutable::scan_query_with_options(&mut store, &query, 1 << 20, true, |_, recheck| {
            assert!(!recheck);
            Ok(())
        })
        .unwrap(),
        1
    );
    assert!(store.fragment_reads > 1);
}

#[test]
fn every_prefix_proof_agrees_with_independent_text_oracle() {
    for seed in 0..24 {
        let words = ["alpha", "beta", "echo", "zulu"];
        let text = (0..40)
            .map(|i| words[((i * i + seed * i + seed * seed) % 7) % 4])
            .collect::<Vec<_>>()
            .join(" ");
        let doc = prepared(&text);
        let analyzed = Analyzed::analyze(&text, AnalysisLimits::default()).unwrap();
        for source in [
            "\"alpha beta\"",
            "\"beta alpha\"",
            "\"echo echo\"",
            "\"zulu alpha echo\"",
            "\"missing zulu\"",
        ] {
            let query = Query::parse(source, QueryLimits::default()).unwrap();
            let expected = oracle::matches(&analyzed, &query, 1 << 20, 1 << 20).unwrap();
            let wanted = source
                .trim_matches('"')
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>();
            for end in 0..=doc.bytes().len() {
                let proof = mutable::phrase_prefix::matches(
                    &doc.bytes()[..end],
                    doc.bytes().len(),
                    doc.token_count(),
                    doc.term_count(),
                    &wanted,
                )
                .unwrap();
                if let Some(actual) = proof {
                    assert_eq!(actual, expected, "{seed}/{source}/{end}");
                }
                if end == doc.bytes().len() {
                    assert_eq!(proof, Some(expected));
                }
            }
        }
    }
}

#[test]
fn prefix_witness_rejects_consumed_corruption_and_leaves_unused_tail_to_verifier() {
    use pin_core::mutable::document::validate;
    let doc = prepared("echo echo echo");
    let wanted = vec!["echo".to_string(), "echo".to_string()];
    let mut bytes = doc.bytes().to_vec();
    let last = bytes.len() - 1;
    bytes[last] = 0;
    assert!(validate(&bytes, 1 << 20).is_err());
    assert_eq!(
        mutable::phrase_prefix::matches(&bytes, bytes.len(), 3, 1, &wanted).unwrap(),
        Some(true)
    );
    bytes[last - 1] = 0;
    assert!(mutable::phrase_prefix::matches(&bytes, bytes.len(), 3, 1, &wanted).is_err());
}
