#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::grouped::{self, GroupSort, SortRecord};
use pin_core::mutable::page::{Page, PageKind};
use pin_core::mutable::{self, CompactMode, PageStore, Stage, document::PreparedDocument};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeSet;

#[derive(Default)]
struct Sort {
    rows: Vec<SortRecord>,
    position: usize,
}

impl GroupSort for Sort {
    fn put(&mut self, rows: &[SortRecord]) -> Result<()> {
        self.rows.extend_from_slice(rows);
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.rows.sort_unstable();
        Ok(())
    }
    fn read(&mut self, output: &mut [SortRecord]) -> Result<usize> {
        let count = output.len().min(self.rows.len() - self.position);
        output[..count].copy_from_slice(&self.rows[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

#[derive(Default)]
struct Store {
    inner: MemoryStore,
    owner_frontier: bool,
    reads: usize,
    owners: usize,
    postings: usize,
    polls: usize,
    cancel_at: Option<usize>,
}

impl PageStore for Store {
    fn owner_frontier(&self) -> bool {
        self.owner_frontier
    }
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        self.reads += 1;
        let page = self.inner.read(block)?;
        self.owners += usize::from(page.kind() == PageKind::Owners);
        self.postings += usize::from(matches!(
            page.kind(),
            PageKind::Postings | PageKind::SealedPostings | PageKind::DirectPostings
        ));
        Ok(page)
    }
    fn extend(&mut self) -> Result<u32> {
        self.inner.extend()
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        self.inner.commit(pages)
    }
    fn remove_owners(&mut self, page: &Page) -> Result<()> {
        self.inner.remove_owners(page)
    }
    fn interrupt(&mut self) -> Result<()> {
        self.polls += 1;
        if self.cancel_at == Some(self.polls) {
            Err(Error::InvalidState)
        } else {
            Ok(())
        }
    }
    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
}

fn tid(index: u32) -> RootTid {
    RootTid::new(
        index / 200,
        (index % 200 + 1) as u16,
        HeapLayout::new(291).unwrap(),
    )
    .unwrap()
}

fn insert(store: &mut impl PageStore, index: u32, text: &str) {
    let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
    let document = PreparedDocument::prepare(&analyzed, 8 << 20).unwrap();
    mutable::insert(store, tid(index), &document).unwrap();
}

fn snapshot(store: &mut impl PageStore) {
    grouped::rebuild(store, &mut Sort::default(), 8 << 20).unwrap();
}

fn scan(store: &mut impl PageStore, text: &str, budget: usize) -> Result<Vec<(RootTid, bool)>> {
    let query = Query::parse(text, QueryLimits::default()).unwrap();
    let mut result = Vec::new();
    let count = grouped::scan_query(store, &query, budget, |root, recheck| {
        result.push((root, recheck));
        Ok(())
    })?;
    assert_eq!(count, result.len() as u64);
    Ok(result)
}

fn exact(store: &mut impl PageStore, source: &str, documents: &[(u32, &str)]) {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let expected: BTreeSet<_> = documents
        .iter()
        .filter_map(|&(index, text)| {
            let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
            oracle::matches(&analyzed, &query, 1 << 20, 1 << 20)
                .unwrap()
                .then_some(tid(index))
        })
        .collect();
    let actual = scan(store, source, 8 << 20).unwrap();
    assert!(actual.iter().all(|(_, recheck)| !recheck), "{source}");
    assert_eq!(
        actual.len(),
        expected.len(),
        "duplicate candidate: {source}"
    );
    assert_eq!(
        actual
            .into_iter()
            .map(|(root, _)| root)
            .collect::<BTreeSet<_>>(),
        expected,
        "{source}"
    );
}

#[test]
fn dense_multi_term_delta_uses_one_owner_frontier_pass() {
    let mut store = Store {
        owner_frontier: true,
        ..Store::default()
    };
    mutable::initialize(&mut store).unwrap();
    let mut documents = Vec::new();
    for index in 0..128 {
        insert(&mut store, index, "a b");
        documents.push((index, "a b"));
    }
    snapshot(&mut store);
    for index in 128..1152 {
        let text = if index % 3 == 0 { "a b c" } else { "a c" };
        insert(&mut store, index, text);
        documents.push((index, text));
    }
    store.inner.events.clear();
    store.owners = 0;
    store.postings = 0;
    exact(&mut store, "a AND b", &documents);
    assert!(store.inner.events.contains(&Stage::OwnerFrontierScan));
    assert!(store.owners > 0);
    assert_eq!(store.postings, 2);
}

#[test]
fn unrelated_long_delta_keeps_term_addressed_frontier() {
    let mut store = Store {
        owner_frontier: true,
        ..Store::default()
    };
    mutable::initialize(&mut store).unwrap();
    for index in 0..128 {
        insert(&mut store, index, "a b");
    }
    snapshot(&mut store);
    for index in 128..1152 {
        insert(&mut store, index, "unrelated filler");
    }
    store.inner.events.clear();
    let result = scan(&mut store, "a AND b", 8 << 20).unwrap();
    assert_eq!(result.len(), 128);
    assert!(!store.inner.events.contains(&Stage::OwnerFrontierScan));
}

#[test]
fn owner_frontier_budget_falls_back_before_emission() {
    let mut store = Store {
        owner_frontier: true,
        ..Store::default()
    };
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, 0, "a b");
    snapshot(&mut store);
    let large = format!("{}b", "a ".repeat(70_000));
    insert(&mut store, 1, &large);
    for index in 2..514 {
        insert(&mut store, index, "a b");
    }
    store.inner.events.clear();
    let result = scan(&mut store, "a AND b", 192 << 10).unwrap();
    assert_eq!(result.len(), 514);
    assert!(result.iter().all(|(_, recheck)| !recheck));
    assert_eq!(
        store
            .inner
            .events
            .iter()
            .filter(|&&stage| stage == Stage::OwnerFrontierScan)
            .count(),
        1
    );
}

#[test]
fn unrelated_writes_do_not_create_boolean_candidates_or_owner_reads() {
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    for index in 0..96 {
        insert(&mut store, index, "alpha beta");
    }
    snapshot(&mut store);
    let queries = [
        "alpha AND beta",
        "beta AND alpha",
        "alpha OR beta",
        "alpha AND NOT missing",
        "missing AND alpha",
    ];
    let mut reference = Vec::new();
    for query in queries {
        store.reads = 0;
        reference.push((scan(&mut store, query, 8 << 20).unwrap(), store.reads));
    }
    let mut previous = 0;
    for size in [1, 1000, 4096] {
        for index in previous..size {
            insert(&mut store, 1000 + index, "unrelated filler");
        }
        previous = size;
        for (query, (expected, reads)) in queries.iter().zip(&reference) {
            store.reads = 0;
            store.owners = 0;
            assert_eq!(
                &scan(&mut store, query, 8 << 20).unwrap(),
                expected,
                "{query}: {size}"
            );
            assert_eq!(store.owners, 0, "{query}: {size}");
            assert!(
                store.reads <= reads + 2,
                "{query}: {size}, reads={}",
                store.reads
            );
        }
    }
}

#[test]
fn boolean_oracle_covers_new_terms_negation_empty_documents_and_tail_suffixes() {
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    let mut documents = Vec::new();
    for index in 0..128 {
        let text = ["", "a", "b", "a b"][index as usize % 4];
        insert(&mut store, index, text);
        documents.push((index, text));
    }
    snapshot(&mut store);
    for index in 128..256 {
        let text = ["", "a", "b", "a b", "c", "a c", "b c", "a b c"][index as usize % 8];
        insert(&mut store, index, text);
        documents.push((index, text));
    }
    for query in [
        "a",
        "c",
        "missing",
        "a AND b",
        "b AND a",
        "a OR c",
        "a AND NOT c",
        "NOT a",
        "NOT missing",
        "NOT NOT c",
        "NOT (a OR b)",
        "a AND a",
        "a AND NOT a",
        "a OR NOT a",
        "(a OR NOT b) AND (c OR NOT a)",
        "(a AND b) OR (c AND NOT b)",
        "NOT (a AND NOT (b OR c))",
        "(missing OR c) AND NOT b",
    ] {
        exact(&mut store, query, &documents);
    }
}

#[test]
fn suffix_spanning_multiple_pages_never_jumps_over_earlier_new_matches() {
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    let mut documents = Vec::new();
    for index in 0..640 {
        insert(&mut store, index, "a b");
        documents.push((index, "a b"));
    }
    snapshot(&mut store);
    for index in 640..1920 {
        let text = if index % 3 == 0 { "a b" } else { "a" };
        insert(&mut store, index, text);
        documents.push((index, text));
    }
    for query in ["a", "a AND b", "a AND NOT b", "a OR b", "NOT NOT a"] {
        exact(&mut store, query, &documents);
    }
}

#[test]
fn unchanged_sealed_and_direct_tails_and_new_mutable_pages_share_the_fence() {
    for mode in [
        CompactMode::Copy,
        CompactMode::RetainSealedPrefix,
        CompactMode::DirectTid,
    ] {
        let mut store = Store::default();
        mutable::initialize(&mut store).unwrap();
        let mut documents = Vec::new();
        for index in 0..96 {
            insert(&mut store, index, "a b");
            documents.push((index, "a b"));
        }
        mutable::compact_with_mode(&mut store, mode).unwrap();
        snapshot(&mut store);
        insert(&mut store, 96, "unrelated");
        documents.push((96, "unrelated"));
        exact(&mut store, "a AND b", &documents);
        for index in 97..160 {
            insert(&mut store, index, "a c");
            documents.push((index, "a c"));
        }
        for query in ["a AND b", "a AND c", "a AND NOT b", "NOT c"] {
            exact(&mut store, query, &documents);
        }
    }
}

#[test]
fn different_incarnations_of_one_tid_never_form_a_conjunction() {
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    snapshot(&mut store);
    insert(&mut store, 0, "a");
    insert(&mut store, 0, "b");
    assert!(scan(&mut store, "a AND b", 8 << 20).unwrap().is_empty());
    mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
    insert(&mut store, 0, "b c");
    exact(&mut store, "a AND c", &[(0, "b c")]);
    exact(&mut store, "b AND c", &[(0, "b c")]);
    exact(&mut store, "NOT a", &[(0, "b c")]);
}

#[test]
fn unpublished_owner_is_excluded_at_every_insert_failure_boundary() {
    let mut base = MemoryStore::default();
    mutable::initialize(&mut base).unwrap();
    insert(&mut base, 0, "a b");
    snapshot(&mut base);
    base.events.clear();
    let analyzed = Analyzed::analyze("a b c", AnalysisLimits::default()).unwrap();
    let document = PreparedDocument::prepare(&analyzed, 8 << 20).unwrap();
    let mut probe = base.clone();
    mutable::insert(&mut probe, tid(1), &document).unwrap();
    for boundary in 0..probe
        .events
        .iter()
        .position(|&stage| stage == Stage::Published)
        .unwrap()
    {
        let mut store = base.clone();
        store.fail_at = Some(boundary);
        assert!(mutable::insert(&mut store, tid(1), &document).is_err());
        store.fail_at = None;
        for query in ["a AND b", "c OR b", "NOT c", "a AND NOT c"] {
            exact(&mut store, query, &[(0, "a b")]);
        }
    }
}

#[test]
fn cancellation_aborts_instead_of_returning_a_partial_success() {
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    snapshot(&mut store);
    for index in 0..64 {
        insert(&mut store, index, "a b");
    }
    store.polls = 0;
    scan(&mut store, "a AND b", 8 << 20).unwrap();
    let polls = store.polls;
    for at in 1..=polls {
        store.polls = 0;
        store.cancel_at = Some(at);
        assert!(scan(&mut store, "a AND b", 8 << 20).is_err(), "poll {at}");
    }
}

#[test]
fn small_budget_and_unsupported_syntax_preserve_the_reference_fallback() {
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, 0, "a b");
    snapshot(&mut store);
    insert(&mut store, 1, "a b");
    insert(&mut store, 2, "unrelated");
    for (query, budget) in [
        ("a AND b", 128 << 10),
        ("\"a b\"", 8 << 20),
        ("a*", 8 << 20),
    ] {
        let parsed = Query::parse(query, QueryLimits::default()).unwrap();
        let mut expected = Vec::new();
        mutable::scan_query_with_recheck(&mut store, &parsed, budget, |root, recheck| {
            expected.push((root, recheck));
            Ok(())
        })
        .unwrap();
        assert_eq!(scan(&mut store, query, budget).unwrap(), expected);
    }
}
