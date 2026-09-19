#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::codec::records::Publication;
use pin_core::error::{Error, Result};
use pin_core::identity::RootTid;
use pin_core::mutable::document::PreparedDocument;
use pin_core::mutable::{self, CountCandidate, PageStore};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeSet;

fn seed(texts: &[&str]) -> MemoryStore {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    for (index, text) in texts.iter().enumerate() {
        let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
        let prepared = PreparedDocument::prepare(&document, 1 << 20).unwrap();
        let root = RootTid::new(index as u32, 1, store.layout()).unwrap();
        mutable::insert(&mut store, root, &prepared).unwrap();
    }
    store
}

fn candidates(store: &mut MemoryStore, source: &str) -> Vec<CountCandidate> {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let mut output = Vec::new();
    let count = mutable::scan_count(store, &query, |candidate| {
        output.push(candidate);
        Ok(())
    })
    .unwrap();
    assert_eq!(count, output.len() as u64);
    let unique: BTreeSet<_> = output
        .iter()
        .map(|c| (c.owner.page, c.owner.slot, c.owner.incarnation))
        .collect();
    assert_eq!(output.len(), unique.len());
    output
}

#[test]
fn count_shape_gate_accepts_only_exact_terms() {
    for (source, expected) in [
        ("alpha", true),
        ("missing", true),
        ("alpha OR beta", false),
        ("alpha AND beta", false),
        ("\"alpha beta\"", false),
        ("alpha*", false),
        ("NOT alpha", false),
        ("", false),
    ] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        assert_eq!(query.is_single_term(), expected, "{source}");
    }
}

#[test]
fn every_query_uses_a_duplicate_free_cover() {
    let texts = ["", "alpha", "beta", "alpha beta", "alpha alpha", "betamax"];
    let mut store = seed(&texts);
    for source in [
        "",
        "missing",
        "alpha",
        "alpha OR beta",
        "alpha OR alpha",
        "alpha AND beta",
        "NOT alpha",
        "\"alpha alpha\"",
        "beta*",
    ] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        let rows = candidates(&mut store, source);
        let roots: BTreeSet<_> = rows
            .iter()
            .map(|candidate| {
                let page = store.read(candidate.owner.page).unwrap();
                page.owner(candidate.owner.slot, store.layout())
                    .unwrap()
                    .root
            })
            .collect();
        for (index, text) in texts.iter().enumerate() {
            let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
            if oracle::matches(&document, &query, 1 << 20, 1 << 20).unwrap() {
                assert!(roots.contains(&RootTid::new(index as u32, 1, store.layout()).unwrap()));
            }
        }
        assert!(rows.iter().all(|candidate| !candidate.sealed_term));
    }
}

#[test]
fn only_sealed_single_term_membership_is_certifiable() {
    let texts = vec!["alpha beta"; 800];
    let mut store = seed(&texts);
    mutable::compact(&mut store).unwrap();
    let rows = candidates(&mut store, "alpha");
    assert_eq!(rows.len(), texts.len());
    assert!(!rows[0].sealed_term);
    assert!(rows[1..].iter().all(|candidate| candidate.sealed_term));
    for source in [
        "alpha OR beta",
        "alpha AND beta",
        "\"alpha beta\"",
        "NOT beta",
        "a*",
    ] {
        assert!(
            candidates(&mut store, source)
                .iter()
                .all(|c| !c.sealed_term)
        );
    }
    let analyzed = Analyzed::analyze("alpha", AnalysisLimits::default()).unwrap();
    let document = PreparedDocument::prepare(&analyzed, 1 << 20).unwrap();
    let root = RootTid::new(9999, 1, store.layout()).unwrap();
    mutable::insert(&mut store, root, &document).unwrap();
    let rows = candidates(&mut store, "alpha");
    assert_eq!(rows.len(), texts.len() + 1);
    assert!(!rows.last().unwrap().sealed_term);
}

#[test]
fn vacuumed_incarnations_never_certify_reused_coordinates() {
    let mut store = seed(&["alpha", "alpha", "beta"]);
    let removed = RootTid::new(1, 1, store.layout()).unwrap();
    mutable::vacuum(&mut store, |root| Ok(root == removed)).unwrap();
    let analyzed = Analyzed::analyze("beta", AnalysisLimits::default()).unwrap();
    let document = PreparedDocument::prepare(&analyzed, 1 << 20).unwrap();
    mutable::insert(&mut store, removed, &document).unwrap();
    for candidate in candidates(&mut store, "alpha") {
        let page = store.read(candidate.owner.page).unwrap();
        let owner = page.owner(candidate.owner.slot, store.layout()).unwrap();
        assert_eq!(owner.reference, candidate.owner);
        if owner.root == removed {
            assert!(!owner.live || owner.publication != Publication::Published);
        }
    }
}

#[test]
fn cancellation_never_returns_partial_accounting() {
    let mut store = seed(&["alpha", "alpha", "alpha"]);
    let query = Query::parse("alpha", QueryLimits::default()).unwrap();
    let mut seen = 0;
    let result = mutable::scan_count(&mut store, &query, |_| -> Result<()> {
        seen += 1;
        if seen == 2 {
            Err(Error::Limit("cancelled"))
        } else {
            Ok(())
        }
    });
    assert_eq!(result, Err(Error::Limit("cancelled")));
    assert_eq!(seen, 2);
}
