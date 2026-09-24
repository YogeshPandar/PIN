#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::grouped::{self, GroupSort, SortRecord};
use pin_core::mutable::page::{GroupSnapshot, NO_BLOCK, Page, PageKind};
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
    fn read(&mut self, out: &mut [SortRecord]) -> Result<usize> {
        let count = out.len().min(self.rows.len() - self.position).min(17);
        out[..count].copy_from_slice(&self.rows[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Work {
    reads: usize,
    posting_reads: usize,
    posting_bytes: usize,
    owner_reads: usize,
}

#[derive(Clone)]
struct Store {
    inner: MemoryStore,
    enabled: bool,
    work: Work,
    polls: usize,
    cancel_at: Option<usize>,
    commit_failure: Option<bool>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            inner: MemoryStore::default(),
            enabled: true,
            work: Work::default(),
            polls: 0,
            cancel_at: None,
            commit_failure: None,
        }
    }
}

impl PageStore for Store {
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn frontier_anchors(&self) -> bool {
        self.enabled
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        let page = self.inner.read(block)?;
        self.work.reads += 1;
        self.work.owner_reads += usize::from(page.kind() == PageKind::Owners);
        if matches!(
            page.kind(),
            PageKind::Postings | PageKind::SealedPostings | PageKind::DirectPostings
        ) {
            self.work.posting_reads += 1;
            self.work.posting_bytes += page.bytes().len();
        }
        Ok(page)
    }
    fn extend(&mut self) -> Result<u32> {
        self.inner.extend()
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        match self.commit_failure.take() {
            Some(false) => Err(Error::InvalidState),
            Some(true) => {
                self.inner.commit(pages)?;
                Err(Error::InvalidState)
            }
            None => self.inner.commit(pages),
        }
    }
    fn remove_owners(&mut self, page: &Page) -> Result<()> {
        self.inner.remove_owners(page)
    }
    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
    fn interrupt(&mut self) -> Result<()> {
        self.polls += 1;
        if self.cancel_at == Some(self.polls) {
            Err(Error::InvalidState)
        } else {
            Ok(())
        }
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

fn document(text: &str) -> PreparedDocument {
    PreparedDocument::prepare(
        &Analyzed::analyze(text, AnalysisLimits::default()).unwrap(),
        8 << 20,
    )
    .unwrap()
}

fn insert(store: &mut Store, index: u32, text: &str) {
    mutable::insert(store, tid(index), &document(text)).unwrap();
}

fn build(store: &mut Store) -> Result<grouped::BuildStats> {
    grouped::rebuild(store, &mut Sort::default(), 8 << 20)
}

fn active(store: &mut Store) -> GroupSnapshot {
    store
        .read(0)
        .unwrap()
        .grouped_state()
        .unwrap()
        .active
        .unwrap()
}

fn scan(store: &mut Store, text: &str) -> Result<Vec<(RootTid, bool)>> {
    let query = Query::parse(text, QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    let count = grouped::scan_query(store, &query, 8 << 20, |root, recheck| {
        rows.push((root, recheck));
        Ok(())
    })?;
    assert_eq!(count, rows.len() as u64);
    Ok(rows)
}

fn exact(store: &mut Store, text: &str, docs: &[(u32, &str)]) {
    let query = Query::parse(text, QueryLimits::default()).unwrap();
    let expected: BTreeSet<_> = docs
        .iter()
        .filter_map(|&(index, text)| {
            let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
            oracle::matches(&analyzed, &query, 1 << 20, 1 << 20)
                .unwrap()
                .then_some(tid(index))
        })
        .collect();
    let actual = scan(store, text).unwrap();
    assert!(actual.iter().all(|(_, recheck)| !recheck), "{text}");
    assert_eq!(actual.len(), expected.len(), "{text}");
    assert_eq!(
        actual
            .into_iter()
            .map(|(root, _)| root)
            .collect::<BTreeSet<_>>(),
        expected,
        "{text}"
    );
}

fn measured(store: &mut Store, query: &str, enabled: bool) -> (Vec<(RootTid, bool)>, Work) {
    store.enabled = enabled;
    store.work = Work::default();
    let rows = scan(store, query).unwrap();
    (rows, store.work)
}

#[test]
fn related_write_work_skips_history_after_an_unrelated_incarnation_gap() {
    for history in [2048, 16384] {
        let mut store = Store::default();
        mutable::initialize(&mut store).unwrap();
        let common = document("alpha beta");
        let rare = document("alpha beta rareplanet");
        for index in 0..history {
            mutable::insert(
                &mut store,
                tid(index),
                if index < 20 { &rare } else { &common },
            )
            .unwrap();
        }
        mutable::compact(&mut store).unwrap();
        build(&mut store).unwrap();
        assert!(active(&mut store).frontier_valid);
        let queries = [
            "rareplanet",
            "alpha AND rareplanet",
            "alpha AND beta",
            "alpha OR rareplanet",
        ];
        for query in queries {
            let old = measured(&mut store, query, false);
            let new = measured(&mut store, query, true);
            assert_eq!(old, new, "fresh: {query}");
        }
        let unrelated = document("unrelated filler");
        for index in history..history + 1000 {
            mutable::insert(&mut store, tid(index), &unrelated).unwrap();
        }
        for query in queries {
            let old = measured(&mut store, query, false);
            let new = measured(&mut store, query, true);
            assert_eq!(old, new, "unchanged: {query}");
        }
        for index in history + 1000..history + 1064 {
            mutable::insert(&mut store, tid(index), &rare).unwrap();
        }
        for query in queries {
            let (old, old_work) = measured(&mut store, query, false);
            let (new, new_work) = measured(&mut store, query, true);
            assert_eq!(new, old, "{query}");
            let rare_query = query == "rareplanet" || query == "alpha AND rareplanet";
            let expected: BTreeSet<_> = (0..if rare_query { 20 } else { history })
                .chain(history + 1000..history + 1064)
                .map(tid)
                .collect();
            assert_eq!(new.len(), expected.len());
            assert!(new.iter().all(|(_, recheck)| !recheck));
            assert_eq!(
                new.into_iter()
                    .map(|(root, _)| root)
                    .collect::<BTreeSet<_>>(),
                expected
            );
            assert_eq!(new_work.owner_reads, old_work.owner_reads);
            assert!(new_work.posting_reads <= 6, "{query}: {new_work:?}");
            if history == 16384 && query != "rareplanet" {
                assert!(
                    new_work.posting_reads < old_work.posting_reads,
                    "{query}: {old_work:?} -> {new_work:?}"
                );
                assert!(
                    new_work.posting_bytes < old_work.posting_bytes,
                    "{query}: {old_work:?} -> {new_work:?}"
                );
            }
            println!("history={history},query={query},legacy={old_work:?},anchors={new_work:?}");
        }
    }
}

#[test]
fn mixed_boolean_oracle_covers_inline_anchors_new_terms_and_multi_page_suffixes() {
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    let mut docs = vec![(0, "single")];
    insert(&mut store, 0, "single");
    for index in 1..256 {
        let text = ["", "a", "b", "a b"][index as usize % 4];
        insert(&mut store, index, text);
        docs.push((index, text));
    }
    build(&mut store).unwrap();
    for index in 256..2304 {
        let text = ["", "a single", "b c", "a b c", "c", "a c", "b", "single"][index as usize % 8];
        insert(&mut store, index, text);
        docs.push((index, text));
    }
    for query in [
        "a AND b",
        "b AND a",
        "a OR c",
        "single AND a",
        "NOT a",
        "NOT missing",
        "a AND NOT c",
        "NOT NOT c",
        "a AND NOT a",
        "a OR NOT a",
        "(a OR NOT b) AND (c OR NOT a)",
        "(a AND b) OR (c AND NOT b)",
        "NOT (a AND NOT (b OR c))",
        "single AND NOT c",
        "c AND NOT single",
    ] {
        exact(&mut store, query, &docs);
    }
}

#[test]
fn empty_snapshots_empty_documents_dead_terms_and_legacy_upgrades_are_readable() {
    for empty_docs in [0, 32] {
        let mut store = Store::default();
        mutable::initialize(&mut store).unwrap();
        for index in 0..empty_docs {
            insert(&mut store, index, "");
        }
        build(&mut store).unwrap();
        let snapshot = active(&mut store);
        assert_eq!(snapshot.frontier_root, Some(NO_BLOCK));
        assert!(snapshot.frontier_valid);
        assert!(!grouped::needs_rebuild(&mut store).unwrap());
        insert(&mut store, 100, "a");
        exact(&mut store, "a", &[(100, "a")]);
    }
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, 0, "a b");
    store.enabled = false;
    build(&mut store).unwrap();
    assert_eq!(active(&mut store).frontier_root, None);
    store.enabled = true;
    assert!(grouped::needs_rebuild(&mut store).unwrap());
    build(&mut store).unwrap();
    assert!(active(&mut store).frontier_valid);
    mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
    build(&mut store).unwrap();
    let snapshot = active(&mut store);
    assert_eq!(snapshot.root, NO_BLOCK);
    assert_ne!(snapshot.frontier_root, Some(NO_BLOCK));
    insert(&mut store, 0, "b");
    exact(&mut store, "a AND b", &[(0, "b")]);
    exact(&mut store, "b", &[(0, "b")]);
    store.enabled = false;
    build(&mut store).unwrap();
    assert_eq!(active(&mut store).frontier_root, None);
}

#[test]
fn compaction_invalidates_before_every_rewrite_and_recovery_boundary() {
    for mode in [
        CompactMode::Copy,
        CompactMode::RetainSealedPrefix,
        CompactMode::DirectTid,
    ] {
        let mut base = Store::default();
        mutable::initialize(&mut base).unwrap();
        let docs: Vec<_> = (0..768).map(|index| (index, "a b")).collect();
        for &(index, text) in &docs {
            insert(&mut base, index, text);
        }
        build(&mut base).unwrap();
        base.inner.events.clear();
        let mut probe = base.clone();
        mutable::compact_with_mode(&mut probe, mode).unwrap();
        assert_eq!(
            probe.inner.events.first(),
            Some(&Stage::FrontierInvalidated)
        );
        assert_eq!(
            probe
                .inner
                .events
                .iter()
                .filter(|&&stage| stage == Stage::FrontierInvalidated)
                .count(),
            1
        );
        for boundary in 0..probe.inner.events.len() {
            let mut store = base.clone();
            store.inner.fail_at = Some(boundary);
            assert!(
                mutable::compact_with_mode(&mut store, mode).is_err(),
                "{mode:?}: {boundary}"
            );
            store.inner.fail_at = None;
            assert!(!active(&mut store).frontier_valid);
            mutable::recover_compaction(&mut store).unwrap();
            exact(&mut store, "a AND b", &docs);
            assert!(grouped::needs_rebuild(&mut store).unwrap());
            build(&mut store).unwrap();
            assert!(active(&mut store).frontier_valid);
            exact(&mut store, "a AND b", &docs);
        }
    }
}

#[test]
fn publication_failures_reclaim_both_catalogs_without_losing_committed_matches() {
    let mut base = Store::default();
    mutable::initialize(&mut base).unwrap();
    // more than 101 distinct terms forces an anchor branch and interleaved flushes.
    for index in 0..240 {
        insert(&mut base, index, &format!("term{index} alpha"));
    }
    build(&mut base).unwrap();
    insert(&mut base, 240, "alpha beta");
    base.inner.events.clear();
    let mut probe = base.clone();
    build(&mut probe).unwrap();
    for boundary in 0..probe.inner.events.len() {
        let mut store = base.clone();
        store.inner.fail_at = Some(boundary);
        assert!(build(&mut store).is_err(), "boundary {boundary}");
        store.inner.fail_at = None;
        let expected: Vec<_> = (0..241).map(|index| (index, "alpha")).collect();
        exact(&mut store, "alpha", &expected);
        build(&mut store).unwrap();
        assert!(active(&mut store).frontier_valid);
        exact(&mut store, "alpha", &expected);
        exact(&mut store, "term239", &[(239, "term239")]);
        mutable::vacuum(&mut store, |_| Ok(false)).unwrap();
        exact(&mut store, "alpha", &expected);
    }
}

#[test]
fn unpublished_writes_and_reused_tids_never_create_false_conjunctions() {
    let mut base = Store::default();
    mutable::initialize(&mut base).unwrap();
    for index in 0..128 {
        insert(&mut base, index, "a b");
    }
    build(&mut base).unwrap();
    base.inner.events.clear();
    let new = document("a b c");
    let mut probe = base.clone();
    mutable::insert(&mut probe, tid(128), &new).unwrap();
    let published = probe
        .inner
        .events
        .iter()
        .position(|&stage| stage == Stage::Published)
        .unwrap();
    for boundary in 0..published {
        let mut store = base.clone();
        store.inner.fail_at = Some(boundary);
        assert!(mutable::insert(&mut store, tid(128), &new).is_err());
        store.inner.fail_at = None;
        let docs: Vec<_> = (0..128).map(|index| (index, "a b")).collect();
        for query in ["a AND b", "a AND c", "NOT c"] {
            exact(&mut store, query, &docs);
        }
    }
    mutable::vacuum(&mut base, |_| Ok(true)).unwrap();
    insert(&mut base, 0, "a");
    insert(&mut base, 0, "c");
    assert!(scan(&mut base, "a AND c").unwrap().is_empty());
    mutable::vacuum(&mut base, |_| Ok(true)).unwrap();
    insert(&mut base, 0, "b c");
    exact(&mut base, "a AND c", &[(0, "b c")]);
    exact(&mut base, "b AND c", &[(0, "b c")]);
    mutable::compact(&mut base).unwrap();
    build(&mut base).unwrap();
    exact(&mut base, "b AND c", &[(0, "b c")]);
}

#[test]
fn legacy_fallback_and_cancellation_remain_correct_with_anchored_storage() {
    let mut store = Store::default();
    mutable::initialize(&mut store).unwrap();
    for index in 0..1024 {
        insert(&mut store, index, "a b");
    }
    build(&mut store).unwrap();
    for index in 1024..2048 {
        insert(&mut store, index, "a b c");
    }
    for (text, budget) in [
        ("a*", 8 << 20),
        ("\"a b\"", 8 << 20),
        ("a AND b", 128 << 10),
    ] {
        let query = Query::parse(text, QueryLimits::default()).unwrap();
        let mut old = Vec::new();
        mutable::scan_query_with_recheck(&mut store, &query, budget, |root, recheck| {
            old.push((root, recheck));
            Ok(())
        })
        .unwrap();
        let mut new = Vec::new();
        grouped::scan_query(&mut store, &query, budget, |root, recheck| {
            new.push((root, recheck));
            Ok(())
        })
        .unwrap();
        assert_eq!(new, old, "{text}");
    }
    store.polls = 0;
    scan(&mut store, "a AND c").unwrap();
    let polls = store.polls;
    for at in 1..=polls {
        store.polls = 0;
        store.cancel_at = Some(at);
        assert!(scan(&mut store, "a AND c").is_err(), "poll {at}");
    }
}

#[test]
fn full_anchor_leaves_and_multi_level_lookups_do_not_require_root_at_journal_tail() {
    for terms in [100, 101, 102, 240] {
        let mut store = Store::default();
        mutable::initialize(&mut store).unwrap();
        for index in 0..terms {
            insert(&mut store, index, &format!("term{index}"));
        }
        let stats = build(&mut store).unwrap();
        assert_eq!(stats.frontier_terms, u64::from(terms));
        assert!(active(&mut store).frontier_valid);
        // the term was inline-only at the fence; its new chain has multiple pages.
        insert(&mut store, terms, "unrelated");
        let term = format!("term{}", terms - 1);
        for index in terms + 1..terms + 1100 {
            insert(&mut store, index, &term);
        }
        let (old, _) = measured(&mut store, &term, false);
        let (new, _) = measured(&mut store, &term, true);
        assert_eq!(old, new);
        assert_eq!(new.len(), 1100);
        assert!(new.iter().all(|(_, recheck)| !recheck));
    }
}

#[test]
fn unknown_metadata_versions_flags_and_missing_anchor_roots_fail_closed() {
    let mut base = Store::default();
    mutable::initialize(&mut base).unwrap();
    for index in 0..1024 {
        insert(&mut base, index, "a b");
    }
    mutable::compact(&mut base).unwrap();
    build(&mut base).unwrap();
    let original = base.inner.pages[0].clone();
    let tail = original.len() - 72;
    assert_eq!(&original[tail..tail + 4], b"PG09");
    assert_eq!(&original[tail + 4..tail + 8], &[2, 0, 1, 0]);
    for (offset, value) in [(4, 3), (6, 2), (28, 0)] {
        let mut store = base.clone();
        store.inner.pages[0][tail + offset] = value;
        if offset == 28 {
            store.inner.pages[0][tail + 28..tail + 32].fill(0);
        }
        assert!(scan(&mut store, "a AND b").is_err());
    }
    insert(&mut base, 1024, "unrelated");
    for index in 1025..1089 {
        insert(&mut base, index, "a b");
    }
    let root = active(&mut base).frontier_root.unwrap();
    // a valid page of another kind must not satisfy a persisted anchor lookup.
    let free = Page::free(root, NO_BLOCK).unwrap();
    base.inner.pages[root as usize] = free.bytes().to_vec();
    assert!(scan(&mut base, "a AND b").is_err());
    base.enabled = false;
    assert_eq!(scan(&mut base, "a AND b").unwrap().len(), 1088);
}

#[test]
fn invalidation_commit_failure_preserves_the_safe_side_of_publication() {
    let mut base = Store::default();
    mutable::initialize(&mut base).unwrap();
    let docs: Vec<_> = (0..768).map(|index| (index, "a b")).collect();
    for &(index, text) in &docs {
        insert(&mut base, index, text);
    }
    build(&mut base).unwrap();
    for persisted in [false, true] {
        let mut store = base.clone();
        store.commit_failure = Some(persisted);
        assert!(mutable::compact(&mut store).is_err());
        assert_eq!(active(&mut store).frontier_valid, !persisted);
        if !persisted {
            assert_eq!(store.inner.pages, base.inner.pages);
        }
        assert!(store.read(0).unwrap().rewrite_journal().unwrap().is_none());
        exact(&mut store, "a AND b", &docs);
        mutable::recover_compaction(&mut store).unwrap();
        mutable::compact(&mut store).unwrap();
        build(&mut store).unwrap();
        exact(&mut store, "a AND b", &docs);
    }
}

#[test]
fn seek_event_proves_selection_and_errors_do_not_report_partial_success() {
    let mut base = Store::default();
    mutable::initialize(&mut base).unwrap();
    for index in 0..1024 {
        insert(&mut base, index, "a b");
    }
    build(&mut base).unwrap();
    for index in 1024..2024 {
        insert(&mut base, index, "unrelated");
    }
    for index in 2024..3048 {
        insert(&mut base, index, "a b");
    }
    base.inner.events.clear();
    let mut probe = base.clone();
    let expected = scan(&mut probe, "a AND b").unwrap();
    let boundary = probe
        .inner
        .events
        .iter()
        .position(|&stage| stage == Stage::FrontierSeek)
        .unwrap();
    base.inner.fail_at = Some(boundary);
    assert!(scan(&mut base, "a AND b").is_err());
    base.inner.fail_at = None;
    assert_eq!(scan(&mut base, "a AND b").unwrap(), expected);
    base.enabled = false;
    base.inner.events.clear();
    assert_eq!(scan(&mut base, "a AND b").unwrap(), expected);
    assert!(!base.inner.events.contains(&Stage::FrontierSeek));
}
