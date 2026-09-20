#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::document::PreparedDocument;
use pin_core::mutable::page::{BUCKETS, NO_BLOCK, Page, PageKind, bucket_for};
use pin_core::mutable::{self, CompactMode, CompactStats, PageStore, Stage};
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeSet;

fn document(text: &str) -> PreparedDocument {
    let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
    PreparedDocument::prepare(&analyzed, 32 << 20).unwrap()
}

fn root(index: u32) -> RootTid {
    RootTid::new(
        index / 200 + 1,
        (index % 200 + 1) as u16,
        HeapLayout::new(291).unwrap(),
    )
    .unwrap()
}

fn append(store: &mut MemoryStore, start: u32, end: u32, text: &str) {
    let document = document(text);
    for index in start..end {
        mutable::insert(store, root(index), &document).unwrap();
    }
}

fn sealed(text: &str) -> MemoryStore {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    append(&mut store, 0, 8500, text);
    mutable::compact(&mut store).unwrap();
    store.events.clear();
    store
}

fn retain<S: PageStore>(store: &mut S) -> Result<CompactStats> {
    mutable::compact_with_mode(store, CompactMode::RetainSealedPrefix)
}

fn rows(store: &mut MemoryStore, text: &str) -> Vec<RootTid> {
    let query = Query::parse(text, QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    mutable::scan_query(store, &query, 1 << 20, |root| {
        rows.push(root);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        rows.len(),
        rows.iter().copied().collect::<BTreeSet<_>>().len()
    );
    rows
}

fn chain(store: &mut MemoryStore, text: &str) -> Vec<Page> {
    let meta = store.read(0).unwrap();
    let (mut block, tail) = meta.bucket(bucket_for(text)).unwrap();
    while block != NO_BLOCK {
        let dictionary = store.read(block).unwrap();
        for entry in dictionary.terms().unwrap() {
            let entry = entry.unwrap();
            if entry.term != text {
                continue;
            }
            let mut pages = Vec::new();
            let mut block = entry.head;
            while block != NO_BLOCK {
                let page = store.read(block).unwrap();
                block = page.next().unwrap();
                pages.push(page);
                assert!(pages.len() <= store.pages.len());
            }
            assert_eq!(pages.last().map_or(NO_BLOCK, Page::block), entry.tail);
            return pages;
        }
        if block == tail {
            break;
        }
        block = dictionary.next().unwrap();
    }
    panic!("missing term {text}");
}

fn integrity(store: &mut MemoryStore) {
    let meta = store.read(0).unwrap();
    assert!(meta.rewrite_journal().unwrap().is_none());
    let mut active = BTreeSet::new();
    for bucket in 0..BUCKETS {
        let (mut block, tail) = meta.bucket(bucket).unwrap();
        while block != NO_BLOCK {
            let dictionary = store.read(block).unwrap();
            for entry in dictionary.terms().unwrap() {
                let entry = entry.unwrap();
                for page in chain(store, entry.term) {
                    assert!(active.insert(page.block()), "duplicate extent ownership");
                    assert_eq!(page.posting_term().unwrap(), entry.reference);
                }
            }
            if block == tail {
                break;
            }
            block = dictionary.next().unwrap();
        }
    }
    let mut free = BTreeSet::new();
    let mut block = meta.free_head().unwrap();
    while block != NO_BLOCK {
        assert!(free.insert(block), "free-list cycle");
        assert!(!active.contains(&block), "reclaimed active extent");
        let page = store.read(block).unwrap();
        assert_eq!(page.kind(), PageKind::Free);
        block = page.next().unwrap();
    }
    for block in 0..store.blocks().unwrap() {
        let page = store.read(block).unwrap();
        page.validate(store.layout()).unwrap();
        assert_eq!(page.kind() == PageKind::Free, free.contains(&block));
    }
}

#[test]
fn retention_preserves_payloads_and_matches_the_copying_reference() {
    let mut retained = sealed("alpha beta");
    let original = chain(&mut retained, "alpha");
    assert!(original.len() >= 3);
    append(&mut retained, 8500, 8800, "alpha gamma");
    let mut copied = retained.clone();
    let reference = mutable::compact(&mut copied).unwrap();
    let stats = retain(&mut retained).unwrap();
    assert_eq!(CompactMode::default(), CompactMode::Copy);
    assert_eq!(reference.retained_pages, 0);
    assert_eq!(stats.retained_pages as usize, original.len() - 1);
    assert_eq!(
        reference.written_pages,
        stats.written_pages + stats.retained_pages
    );
    assert_eq!(
        reference.reclaimed_pages,
        stats.reclaimed_pages + stats.retained_pages
    );
    let retained_count = original.len() - 1;
    for (index, original) in original[..retained_count].iter().enumerate() {
        let actual = retained.read(original.block()).unwrap();
        let mut expected = original.clone();
        expected.set_next(actual.next().unwrap()).unwrap();
        assert_eq!(actual.bytes(), expected.bytes());
        if index + 1 < retained_count {
            assert_eq!(actual.next().unwrap(), original.next().unwrap());
        }
    }
    for query in [
        "alpha",
        "beta",
        "gamma",
        "alpha AND beta",
        "beta OR gamma",
        "NOT missing",
    ] {
        assert_eq!(
            rows(&mut retained, query),
            rows(&mut copied, query),
            "{query}"
        );
    }
    integrity(&mut retained);
}

#[test]
fn repeated_small_appends_coalesce_the_sealed_tail() {
    let mut retained = sealed("alpha");
    for index in 8500..8512 {
        append(&mut retained, index, index + 1, "alpha");
        let mut copied = retained.clone();
        mutable::compact(&mut copied).unwrap();
        let stats = retain(&mut retained).unwrap();
        assert!(stats.retained_pages > 0);
        assert_eq!(
            chain(&mut retained, "alpha").len(),
            chain(&mut copied, "alpha").len()
        );
        assert_eq!(rows(&mut retained, "alpha"), rows(&mut copied, "alpha"));
    }
    let before = retained.pages.clone();
    assert_eq!(retain(&mut retained).unwrap(), CompactStats::default());
    assert_eq!(retained.pages, before);
    integrity(&mut retained);
}

#[test]
fn dead_prefix_and_suffix_owners_match_copying_and_never_resurrect() {
    let baseline = sealed("alpha beta");
    for dead in [1, 8400] {
        let mut retained = baseline.clone();
        mutable::vacuum(&mut retained, |candidate| Ok(candidate == root(dead))).unwrap();
        mutable::insert(&mut retained, root(dead), &document("gamma")).unwrap();
        let mut copied = retained.clone();
        let reference = mutable::compact(&mut copied).unwrap();
        let stats = retain(&mut retained).unwrap();
        assert_eq!(stats.removed_postings, reference.removed_postings);
        assert_eq!(stats.removed_postings, 2);
        if dead == 1 {
            assert_eq!(stats.retained_pages, 0);
        } else {
            assert!(stats.retained_pages > 0);
        }
        assert!(!rows(&mut retained, "alpha").contains(&root(dead)));
        for query in ["alpha", "beta", "gamma", "alpha OR gamma", "alpha AND beta"] {
            assert_eq!(rows(&mut retained, query), rows(&mut copied, query));
        }
        integrity(&mut retained);
    }
    let mut empty = baseline;
    mutable::vacuum(&mut empty, |_| Ok(true)).unwrap();
    let stats = retain(&mut empty).unwrap();
    assert_eq!(stats.written_pages, 0);
    assert_eq!(stats.retained_pages, 0);
    assert!(rows(&mut empty, "alpha").is_empty());
    assert!(chain(&mut empty, "alpha").is_empty());
    integrity(&mut empty);
}

#[test]
fn hash_collisions_keep_independent_prefixes_on_one_dictionary_page() {
    assert_eq!(bucket_for("aav"), bucket_for("ala"));
    let mut retained = sealed("aav ala");
    append(&mut retained, 8500, 8600, "aav ala");
    let mut copied = retained.clone();
    mutable::compact(&mut copied).unwrap();
    let stats = retain(&mut retained).unwrap();
    assert_eq!(stats.rewritten_terms, 2);
    assert!(stats.retained_pages >= 4);
    for query in ["aav", "ala", "aav AND ala", "aav OR ala"] {
        assert_eq!(rows(&mut retained, query), rows(&mut copied, query));
    }
    integrity(&mut retained);
}

struct CommitFault {
    inner: MemoryStore,
    stop: Option<(usize, bool)>,
    commits: usize,
    three_page_publications: usize,
}

impl PageStore for CommitFault {
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        self.inner.read(block)
    }
    fn extend(&mut self) -> Result<u32> {
        self.inner.extend()
    }
    fn remove_owners(&mut self, page: &Page) -> Result<()> {
        self.commit(&[page])
    }
    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        let current = self.commits;
        self.commits += 1;
        if pages.iter().any(|page| page.kind() == PageKind::Dictionary) {
            assert_eq!(pages.len(), 3);
            self.three_page_publications += 1;
        }
        if self.stop == Some((current, false)) {
            return Err(Error::InvalidState);
        }
        self.inner.commit(pages)?;
        if self.stop == Some((current, true)) {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
}

#[test]
fn failures_before_and_after_every_wal_batch_keep_reachable_pages_owned() {
    let mut baseline = sealed("alpha beta");
    append(&mut baseline, 8500, 8700, "alpha beta");
    let mut complete = CommitFault {
        inner: baseline.clone(),
        stop: None,
        commits: 0,
        three_page_publications: 0,
    };
    retain(&mut complete).unwrap();
    assert_eq!(complete.three_page_publications, 2);
    let expected = rows(&mut baseline, "alpha");
    for point in 0..complete.commits {
        for after in [false, true] {
            let mut fault = CommitFault {
                inner: baseline.clone(),
                stop: Some((point, after)),
                commits: 0,
                three_page_publications: 0,
            };
            assert!(retain(&mut fault).is_err(), "point {point}, after {after}");
            assert_eq!(rows(&mut fault.inner, "alpha"), expected);
            fault.stop = None;
            mutable::recover_compaction(&mut fault).unwrap();
            assert_eq!(mutable::recover_compaction(&mut fault).unwrap(), 0);
            mutable::vacuum(&mut fault, |_| Ok(false)).unwrap();
            retain(&mut fault).unwrap();
            assert_eq!(rows(&mut fault.inner, "alpha"), expected);
            assert_eq!(rows(&mut fault.inner, "beta"), expected);
            integrity(&mut fault.inner);
        }
    }
}

#[test]
fn every_event_failure_recovers_and_interrupted_retirement_keeps_the_prefix() {
    let mut baseline = sealed("alpha");
    let prefix = chain(&mut baseline, "alpha");
    append(&mut baseline, 8500, 8800, "alpha");
    baseline.events.clear();
    let mut complete = baseline.clone();
    retain(&mut complete).unwrap();
    assert!(complete.events.contains(&Stage::ReplacementPublished));
    assert!(complete.events.contains(&Stage::SegmentReclaimed));
    let expected = rows(&mut baseline, "alpha");
    for point in 0..complete.events.len() {
        let mut fault = baseline.clone();
        fault.fail_at = Some(point);
        assert!(retain(&mut fault).is_err());
        fault.fail_at = None;
        assert_eq!(rows(&mut fault, "alpha"), expected);
        mutable::recover_compaction(&mut fault).unwrap();
        mutable::vacuum(&mut fault, |_| Ok(false)).unwrap();
        retain(&mut fault).unwrap();
        assert_eq!(rows(&mut fault, "alpha"), expected);
        for page in &prefix[..prefix.len() - 1] {
            assert_eq!(
                fault.read(page.block()).unwrap().kind(),
                PageKind::SealedPostings
            );
        }
        integrity(&mut fault);
    }
}

#[test]
fn corrupted_sealed_chain_is_rejected_before_publication() {
    let mut store = sealed("alpha");
    append(&mut store, 8500, 8501, "alpha");
    let pages = chain(&mut store, "alpha");
    let mut second = pages[1].clone();
    second.set_next(pages[0].block()).unwrap();
    store.commit(&[&second]).unwrap();
    let before = store.pages.clone();
    assert!(retain(&mut store).is_err());
    assert_eq!(store.pages, before);
}
