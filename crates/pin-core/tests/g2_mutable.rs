#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::candidate::CandidatePlan;
use pin_core::codec::records::Publication;
use pin_core::identity::RootTid;
use pin_core::mutable::document::{PreparedDocument, validate};
use pin_core::mutable::page::{NO_BLOCK, OwnerChange, Page, PageKind, bucket_for};
use pin_core::mutable::{self, PageStore, Stage};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeSet;

fn prepared(text: &str) -> PreparedDocument {
    let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
    PreparedDocument::prepare(&document, 32 << 20).unwrap()
}

fn root(store: &MemoryStore, index: u32) -> RootTid {
    RootTid::new(index / 200 + 1, (index % 200 + 1) as u16, store.layout()).unwrap()
}

fn candidates(store: &mut MemoryStore, source: &str) -> BTreeSet<RootTid> {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let plan = CandidatePlan::build(&query, 1 << 20).unwrap();
    let mut result = BTreeSet::new();
    mutable::scan(store, &plan, |root| { result.insert(root); Ok(()) }).unwrap();
    result
}

#[test]
fn inline_payloads_pack_without_moving_owner_identities() {
    let store = MemoryStore::default();
    let layout = store.layout();
    let payload = prepared("alpha beta alpha");
    assert_eq!(validate(payload.bytes(), 4096).unwrap(), (3, 2));
    let mut page = Page::owners(1).unwrap();
    let mut references = Vec::new();
    for index in 0..1000 {
        let incarnation = pin_core::identity::Incarnation::new(index as u64 + 1).unwrap();
        let Some(reference) = page.append_owner(incarnation, root(&store, index), 3, 2, payload.bytes()).unwrap() else {
            break;
        };
        page.change_owner(reference, OwnerChange::PayloadReady(NO_BLOCK), layout).unwrap();
        page.change_owner(reference, OwnerChange::Publish, layout).unwrap();
        references.push(reference);
    }
    assert!(references.len() > 50);
    page.validate(layout).unwrap();
    for reference in references {
        let owner = page.owner(reference.slot, layout).unwrap();
        assert_eq!(owner.reference, reference);
        assert_eq!(owner.inline, payload.bytes());
        assert!(owner.live);
    }
}

#[test]
fn lossy_candidates_plus_recheck_equal_the_independent_oracle() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let texts = ["", "alpha", "alpha beta", "beta alpha alpha", "alphabet", "é BETA", "missing"];
    let mut documents = Vec::new();
    for index in 0..1100 {
        let text = texts[index as usize % texts.len()];
        let tid = root(&store, index);
        mutable::insert(&mut store, tid, &prepared(text)).unwrap();
        documents.push((tid, Analyzed::analyze(text, AnalysisLimits::default()).unwrap()));
    }
    for source in ["alpha", "alpha OR beta", "alpha AND beta", "NOT alpha", "alpha*", "\"alpha alpha\"", "NOT (alpha OR beta)", "", "missing AND alpha"] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        let actual = candidates(&mut store, source);
        for (root, document) in &documents {
            let exact = oracle::matches(document, &query, 1 << 20, 1 << 20).unwrap();
            assert!(!exact || actual.contains(root), "{source:?} missed {root:?}");
        }
    }
    let stats = mutable::vacuum(&mut store, |_| Ok(false)).unwrap();
    assert_eq!(stats.live_documents, 1100);
    assert_eq!(stats.removed_documents, 0);
}

#[test]
fn old_terms_cannot_resurrect_when_the_same_heap_slot_is_reused() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let tid = root(&store, 0);
    let old = mutable::insert(&mut store, tid, &prepared("alpha")).unwrap();
    let stats = mutable::vacuum(&mut store, |candidate| Ok(candidate == tid)).unwrap();
    assert_eq!(stats.removed_documents, 1);
    let new = mutable::insert(&mut store, tid, &prepared("beta")).unwrap();
    assert_ne!(old, new);
    assert!(candidates(&mut store, "alpha").is_empty());
    assert_eq!(candidates(&mut store, "beta"), BTreeSet::from([tid]));
    let mut owner_page = store.read(old.page).unwrap();
    assert!(owner_page.change_owner(old, OwnerChange::Publish, store.layout()).is_err());
    assert!(!owner_page.owner(old.slot, store.layout()).unwrap().live);
}

#[test]
fn every_insertion_boundary_recovers_without_half_published_matches() {
    let mut baseline = MemoryStore::default();
    mutable::initialize(&mut baseline).unwrap();
    let committed = root(&baseline, 0);
    mutable::insert(&mut baseline, committed, &prepared("stable alpha")).unwrap();
    let attempted = root(&baseline, 1);
    let document = prepared(&"alpha beta gamma ".repeat(5000));
    let mut successful = baseline.clone();
    successful.events.clear();
    mutable::insert(&mut successful, attempted, &document).unwrap();
    assert!(successful.events.contains(&Stage::FragmentStored));
    for point in 0..successful.events.len() {
        let mut crashed = baseline.clone();
        crashed.events.clear();
        crashed.fail_at = Some(point);
        assert!(mutable::insert(&mut crashed, attempted, &document).is_err());
        crashed.fail_at = None;
        let published = crashed.events.last() == Some(&Stage::Published);
        assert_eq!(candidates(&mut crashed, "beta").contains(&attempted), published);
        assert_eq!(candidates(&mut crashed, "gamma").contains(&attempted), published);
        assert!(candidates(&mut crashed, "stable").contains(&committed));
        let stats = mutable::vacuum(&mut crashed, |tid| Ok(tid == attempted)).unwrap();
        assert_eq!(stats.live_documents, 1);
        assert!(!candidates(&mut crashed, "beta").contains(&attempted));
        for block in 1..crashed.blocks().unwrap() {
            let page = crashed.read(block).unwrap();
            assert_ne!(page.kind(), PageKind::Zero);
            if page.kind() == PageKind::Fragment {
                let (reference, _, _) = page.fragment_data().unwrap();
                let owners = crashed.read(reference.page).unwrap();
                let owner = owners.owner(reference.slot, crashed.layout()).unwrap();
                assert_eq!(owner.publication, Publication::Published);
                assert!(owner.live);
            }
        }
        // a second pass must not count or free anything twice.
        let second = mutable::vacuum(&mut crashed, |_| Ok(false)).unwrap();
        assert_eq!(second.removed_documents, 0);
        assert_eq!(second.reclaimed_pages, 0);
    }
}

#[test]
fn vacuum_interruptions_are_idempotent_and_freed_payload_pages_are_reused() {
    let mut baseline = MemoryStore::default();
    mutable::initialize(&mut baseline).unwrap();
    let tid = root(&baseline, 0);
    let document = prepared(&"alpha beta ".repeat(6000));
    mutable::insert(&mut baseline, tid, &document).unwrap();
    let mut complete = baseline.clone();
    complete.events.clear();
    let stats = mutable::vacuum(&mut complete, |_| Ok(true)).unwrap();
    assert!(stats.reclaimed_pages >= 2);
    for point in 0..complete.events.len() {
        let mut interrupted = baseline.clone();
        interrupted.events.clear();
        interrupted.fail_at = Some(point);
        assert!(mutable::vacuum(&mut interrupted, |_| Ok(true)).is_err());
        interrupted.fail_at = None;
        mutable::vacuum(&mut interrupted, |_| Ok(true)).unwrap();
        assert!(candidates(&mut interrupted, "alpha").is_empty());
        let before = interrupted.blocks().unwrap();
        let next_tid = root(&interrupted, 1);
        mutable::insert(&mut interrupted, next_tid, &document).unwrap();
        // two new posting pages hold second occurrences; fragments come from free pages.
        assert!(interrupted.blocks().unwrap() <= before + 2);
        assert_eq!(candidates(&mut interrupted, "beta"), BTreeSet::from([next_tid]));
    }
}

#[test]
fn exact_term_comparison_survives_hash_collisions_and_dictionary_overflow() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let prefix = "x".repeat(900);
    let mut terms = Vec::new();
    for index in 0..100_000 {
        let term = format!("{prefix}{index}");
        if bucket_for(&term) == 0 {
            terms.push(term);
            if terms.len() == 25 { break; }
        }
    }
    assert_eq!(terms.len(), 25);
    for (index, term) in terms.iter().enumerate() {
        let tid = root(&store, index as u32);
        mutable::insert(&mut store, tid, &prepared(term)).unwrap();
    }
    for (index, term) in terms.iter().enumerate() {
        let tid = root(&store, index as u32);
        assert_eq!(candidates(&mut store, term), BTreeSet::from([tid]));
    }
}

#[test]
fn referenced_zero_pages_fail_closed_instead_of_entering_the_free_list() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let tid = root(&store, 0);
    mutable::insert(&mut store, tid, &prepared("alpha")).unwrap();
    let meta = store.read(0).unwrap();
    let (dictionary, _) = meta.bucket(bucket_for("alpha")).unwrap();
    store.pages[dictionary as usize].clear();
    assert!(mutable::vacuum(&mut store, |_| Ok(false)).is_err());
    assert_eq!(store.read(0).unwrap().free_head().unwrap(), NO_BLOCK);
}

#[test]
fn document_validation_rejects_cross_term_position_reuse_and_trailing_bytes() {
    let payload = prepared("alpha beta");
    assert_eq!(validate(payload.bytes(), 4096).unwrap(), (2, 2));
    let mut corrupt = payload.bytes().to_vec();
    let last = corrupt.len() - 1;
    corrupt[last] = 0;
    assert!(validate(&corrupt, 4096).is_err());
    let mut corrupt = payload.bytes().to_vec();
    corrupt.push(0);
    assert!(validate(&corrupt, 4096).is_err());
    assert!(validate(payload.bytes(), 0).is_err());
    assert!(PreparedDocument::prepare(&Analyzed::analyze("a", AnalysisLimits::default()).unwrap(), 0).is_err());
}
