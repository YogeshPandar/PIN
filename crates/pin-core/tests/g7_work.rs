#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::codec::records::Publication;
use pin_core::identity::RootTid;
use pin_core::mutable::document::PreparedDocument;
use pin_core::mutable::work::{WORK_WORDS, WorkBatch, WorkState};
use pin_core::mutable::{self, CountCandidate, PageStore};
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeSet;

fn query(text: &str) -> Query {
    Query::parse(text, QueryLimits::default()).unwrap()
}

fn corpus(count: u32) -> MemoryStore {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    for id in 0..count {
        let text = if id % 2 == 0 {
            "alpha beta"
        } else {
            "alpha gamma"
        };
        let analysis = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
        let document = PreparedDocument::prepare(&analysis, 32 << 20).unwrap();
        let root = RootTid::new(id / 200, (id % 200 + 1) as u16, store.layout()).unwrap();
        mutable::insert(&mut store, root, &document).unwrap();
    }
    store
}

fn drain(store: &mut MemoryStore, query: &Query, workers: usize) -> Vec<CountCandidate> {
    let mut shared = WorkState::capture(store, query, 1 << 20).unwrap();
    let mut results = Vec::new();
    while shared != WorkState::DONE {
        let snapshot = WorkState::from_words(shared.words()).unwrap();
        let mut prepared = Vec::new();
        for _ in 0..workers {
            prepared.push(snapshot.prepare(store).unwrap().unwrap());
        }
        // all contenders read the same page before one compare-and-replace wins.
        let winner = results.len() % workers;
        for offset in 0..workers {
            let participant = (winner + offset) % workers;
            let (successor, batch) = &prepared[participant];
            if shared == snapshot {
                shared = *successor;
                batch
                    .for_each(store.layout(), |candidate| {
                        results.push(candidate);
                        Ok(())
                    })
                    .unwrap();
            } else {
                assert_ne!(shared, snapshot);
            }
        }
    }
    results
}

fn roots(
    store: &mut MemoryStore,
    candidates: &[CountCandidate],
    query: &Query,
) -> BTreeSet<RootTid> {
    let mut rows = BTreeSet::new();
    let mut owners = BTreeSet::new();
    for candidate in candidates {
        let reference = candidate.owner;
        assert!(owners.insert((reference.page, reference.slot, reference.incarnation)));
        let page = store.read(reference.page).unwrap();
        let owner = page.owner(reference.slot, store.layout()).unwrap();
        assert_eq!(owner.reference, reference);
        if !owner.live || owner.publication != Publication::Published {
            continue;
        }
        let id = owner.root.block() * 200 + u32::from(owner.root.offset()) - 1;
        let text = if id % 2 == 0 {
            "alpha beta"
        } else {
            "alpha gamma"
        };
        let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
        if pin_core::oracle::matches(&document, query, 1 << 20, 1_000_000).unwrap() {
            assert!(rows.insert(owner.root));
        }
    }
    rows
}

#[test]
fn competing_claims_cover_each_owner_once() {
    let mut store = corpus(1024);
    for text in [
        "alpha",
        "beta",
        "alpha AND beta",
        "beta OR gamma",
        "NOT missing",
        "alph*",
        "\"alpha beta\"",
    ] {
        let query = query(text);
        let serial = drain(&mut store, &query, 1);
        let expected = roots(&mut store, &serial, &query);
        assert!(!expected.is_empty(), "{text}");
        for workers in [2, 3, 7] {
            let parallel = drain(&mut store, &query, workers);
            assert_eq!(parallel, serial, "{text}: {workers}");
            assert_eq!(roots(&mut store, &parallel, &query), expected);
        }
    }
}

#[test]
fn only_exact_sealed_terms_offer_a_vm_candidate() {
    let mut store = corpus(5000);
    mutable::compact(&mut store).unwrap();
    let term = drain(&mut store, &query("alpha"), 3);
    assert_eq!(term.len(), 5000);
    assert!(!term[0].sealed_term);
    assert!(term.iter().skip(1).any(|candidate| candidate.sealed_term));
    for text in [
        "alpha AND beta",
        "\"alpha beta\"",
        "beta OR gamma",
        "NOT missing",
    ] {
        let candidates = drain(&mut store, &query(text), 3);
        assert!(candidates.iter().all(|candidate| !candidate.sealed_term));
    }
}

#[test]
fn removed_owners_do_not_reappear_after_a_claim() {
    let mut store = corpus(512);
    let query = query("alpha");
    let candidates = drain(&mut store, &query, 4);
    mutable::vacuum(&mut store, |root| Ok(root.offset() % 2 == 0)).unwrap();
    let live = roots(&mut store, &candidates, &query);
    assert_eq!(live.len(), 256);
    assert!(live.iter().all(|root| root.offset() % 2 == 1));
}

#[test]
fn empty_and_missing_queries_produce_no_work() {
    let mut store = corpus(12);
    for text in ["", "missing"] {
        let state = WorkState::capture(&mut store, &query(text), 1 << 20).unwrap();
        assert_eq!(state, WorkState::DONE);
        assert!(state.prepare(&mut store).unwrap().is_none());
    }
}

#[test]
fn malformed_shared_words_fail_before_work() {
    let mut store = corpus(32);
    let valid = WorkState::capture(&mut store, &query("alpha"), 1 << 20)
        .unwrap()
        .words();
    for (field, value) in [
        (0, 4),
        (3, 0),
        (9, 0),
        (10, 2),
        (6, u64::MAX),
        (7, 65536),
        (8, 0),
    ] {
        let mut words = valid;
        words[field] = value;
        assert!(WorkState::from_words(words).is_err(), "field {field}");
    }
    let mut words = [0; WORK_WORDS];
    words[5] = 1;
    assert!(WorkState::from_words(words).is_err());
}

#[test]
fn work_is_bounded_and_restart_does_not_reuse_completion() {
    assert_eq!(std::mem::size_of::<WorkState>(), WORK_WORDS * 8);
    assert!(std::mem::size_of::<WorkBatch>() < 16 * 1024);
    let mut store = corpus(120);
    let alpha = drain(&mut store, &query("alpha"), 3);
    let beta = drain(&mut store, &query("beta"), 3);
    assert_eq!(alpha.len(), 120);
    assert_eq!(beta.len(), 60);
    assert_eq!(drain(&mut store, &query("alpha"), 2), alpha);
}
