#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::grouped::{self, GroupSort, SortRecord};
use pin_core::mutable::page::{GroupPageKind, NO_BLOCK, Page, PageKind};
use pin_core::mutable::{self, PageStore, Stage, document::PreparedDocument};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Sort {
    rows: Vec<SortRecord>,
    position: usize,
    finished: bool,
}

impl GroupSort for Sort {
    fn put(&mut self, rows: &[SortRecord]) -> Result<()> {
        assert!(!self.finished);
        self.rows.extend_from_slice(rows);
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        assert!(!self.finished);
        self.rows.sort_unstable();
        self.finished = true;
        Ok(())
    }
    fn read(&mut self, out: &mut [SortRecord]) -> Result<usize> {
        assert!(self.finished);
        let count = out.len().min(self.rows.len() - self.position).min(17);
        out[..count].copy_from_slice(&self.rows[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

fn root(block: u32, offset: u16) -> RootTid {
    RootTid::new(block, offset, HeapLayout::new(291).unwrap()).unwrap()
}

fn insert(store: &mut impl PageStore, root: RootTid, text: &str) {
    let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
    let document = PreparedDocument::prepare(&analyzed, 8 << 20).unwrap();
    mutable::insert(store, root, &document).unwrap();
}

fn build(store: &mut impl PageStore) -> Result<grouped::BuildStats> {
    grouped::rebuild(store, &mut Sort::default(), 8 << 20)
}

fn raw(store: &mut impl PageStore, text: &str, budget: usize) -> Result<Vec<(RootTid, bool)>> {
    let query = Query::parse(text, QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    let count = grouped::scan_query(store, &query, budget, |root, recheck| {
        rows.push((root, recheck));
        Ok(())
    })?;
    assert_eq!(count, rows.len() as u64);
    Ok(rows)
}

fn check(store: &mut impl PageStore, docs: &BTreeMap<RootTid, &str>, source: &str) {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let rows = raw(store, source, 8 << 20).unwrap();
    let mut expected = BTreeSet::new();
    for (&root, &text) in docs {
        let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
        if oracle::matches(&document, &query, 1 << 20, 1 << 20).unwrap() {
            expected.insert(root);
        }
    }
    let mut actual = BTreeSet::new();
    for (root, recheck) in rows {
        if !docs.contains_key(&root) {
            continue;
        }
        assert!(
            recheck || expected.contains(&root),
            "false proof: {source}, {root:?}"
        );
        if !recheck || expected.contains(&root) {
            actual.insert(root);
        }
    }
    assert_eq!(actual, expected, "missing match: {source}");
}

fn free_pages(store: &mut MemoryStore) -> BTreeSet<u32> {
    let mut result = BTreeSet::new();
    let mut block = store.read(0).unwrap().free_head().unwrap();
    while block != NO_BLOCK {
        assert!(result.insert(block));
        let page = store.read(block).unwrap();
        assert_eq!(page.kind(), PageKind::Free);
        block = page.next().unwrap();
    }
    for block in 1..store.blocks().unwrap() {
        assert_eq!(
            result.contains(&block),
            store.read(block).unwrap().kind() == PageKind::Free
        );
    }
    result
}

#[test]
fn complete_generation_boolean_matches_equal_the_independent_owner_oracle() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let mut docs = BTreeMap::new();
    for (index, text) in ["", "a", "b", "c", "a b", "b a", "a c", "a b c", "alphabet"]
        .iter()
        .enumerate()
    {
        let coordinate = root((index as u32 / 3) * 256 + 1, (index % 3 + 1) as u16);
        insert(&mut store, coordinate, text);
        docs.insert(coordinate, *text);
    }
    let stats = build(&mut store).unwrap();
    assert_eq!(stats.documents, docs.len() as u64);
    let atoms = ["a", "b", "missing", "NOT a", "NOT (a OR b)"];
    for left in atoms {
        check(&mut store, &docs, left);
        for right in atoms {
            for op in ["AND", "OR"] {
                let query = format!("({left}) {op} ({right})");
                check(&mut store, &docs, &query);
                assert!(
                    raw(&mut store, &query, 8 << 20)
                        .unwrap()
                        .iter()
                        .all(|(_, recheck)| !recheck)
                );
                check(&mut store, &docs, &format!("NOT ({query})"));
            }
        }
    }
    for query in ["a*", "\"a b\"", "NOT \"a b\"", "", "a OR (b AND c)"] {
        check(&mut store, &docs, query);
    }
}

#[test]
fn new_writes_are_searchable_before_another_snapshot() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let mut docs = BTreeMap::from([(root(1, 1), "a"), (root(256, 291), "b")]);
    for (&root, &text) in &docs {
        insert(&mut store, root, text);
    }
    build(&mut store).unwrap();
    insert(&mut store, root(1, 2), "a b newterm");
    docs.insert(root(1, 2), "a b newterm");
    for query in ["a AND b", "newterm", "NOT a", "a OR b", "NOT missing"] {
        check(&mut store, &docs, query);
    }
    assert!(
        raw(&mut store, "newterm", 8 << 20)
            .unwrap()
            .contains(&(root(1, 2), false))
    );
    build(&mut store).unwrap();
    assert_eq!(
        raw(&mut store, "a AND b", 8 << 20).unwrap(),
        vec![(root(1, 2), false)]
    );
    free_pages(&mut store);
}

#[test]
fn vacuum_clears_old_incarnations_before_slot_reuse() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, root(1, 1), "a");
    insert(&mut store, root(2, 1), "a b");
    build(&mut store).unwrap();
    mutable::vacuum(&mut store, |tid| Ok(tid == root(1, 1))).unwrap();
    insert(&mut store, root(1, 1), "b");
    let docs = BTreeMap::from([(root(1, 1), "b"), (root(2, 1), "a b")]);
    check(&mut store, &docs, "a AND b");
    assert!(
        !raw(&mut store, "a AND b", 8 << 20)
            .unwrap()
            .contains(&(root(1, 1), false))
    );
    build(&mut store).unwrap();
    assert_eq!(
        raw(&mut store, "a AND b", 8 << 20).unwrap(),
        vec![(root(2, 1), false)]
    );
    assert_eq!(
        mutable::vacuum(&mut store, |_| Ok(false))
            .unwrap()
            .removed_documents,
        0
    );
    free_pages(&mut store);
}

#[test]
fn conflicting_live_incarnations_never_publish_a_new_snapshot() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, root(1, 1), "a");
    build(&mut store).unwrap();
    let old = store.read(0).unwrap().grouped_state().unwrap().active;
    insert(&mut store, root(1, 1), "b");
    assert_eq!(build(&mut store), Err(Error::DuplicateDocument));
    assert_eq!(store.read(0).unwrap().grouped_state().unwrap().active, old);
    assert!(
        !raw(&mut store, "a AND b", 8 << 20)
            .unwrap()
            .contains(&(root(1, 1), false))
    );
}

#[test]
fn multi_leaf_catalog_iteration_and_seek_keep_group_boundaries() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let mut docs = BTreeMap::new();
    for index in 0..130 {
        let tid = root(index * 256 + 255, 291);
        let text = if index % 7 == 0 { "a b" } else { "a" };
        insert(&mut store, tid, text);
        docs.insert(tid, text);
    }
    build(&mut store).unwrap();
    for query in [
        "a",
        "b",
        "a AND b",
        "a AND NOT b",
        "NOT missing",
        "missing OR b",
    ] {
        check(&mut store, &docs, query);
    }
}

#[test]
fn multi_page_liveness_keeps_directory_widths_and_retirement_is_idempotent() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    for block in 0..256 {
        insert(&mut store, root(block, 291), "wide");
    }
    build(&mut store).unwrap();
    let before: Vec<_> = (0..store.pages.len())
        .filter_map(|block| {
            let page = store.read(block as u32).ok()?;
            (page.kind() == PageKind::Grouped
                && page.group_identity().ok()?.1 == GroupPageKind::Liveness)
                .then_some((block, page.bytes().len()))
        })
        .collect();
    assert_eq!(before.len(), 2);
    mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
    assert!(
        raw(&mut store, "wide OR NOT wide", 8 << 20)
            .unwrap()
            .is_empty()
    );
    for (block, len) in before {
        assert_eq!(store.read(block as u32).unwrap().bytes().len(), len);
    }
    mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
}

#[derive(Clone)]
struct FaultStore {
    inner: MemoryStore,
    commits: usize,
    fail: Option<(usize, bool)>,
}

impl PageStore for FaultStore {
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
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        let index = self.commits;
        self.commits += 1;
        if self.fail == Some((index, false)) {
            return Err(Error::InvalidState);
        }
        self.inner.commit(pages)?;
        if self.fail == Some((index, true)) {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
    fn remove_owners(&mut self, page: &Page) -> Result<()> {
        self.commit(&[page])
    }
    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
}

#[test]
fn every_atomic_publication_and_retirement_boundary_is_restartable() {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    for (tid, text) in [
        (root(1, 1), "a"),
        (root(257, 1), "b"),
        (root(512, 291), "a b"),
    ] {
        insert(&mut inner, tid, text);
    }
    build(&mut inner).unwrap();
    insert(&mut inner, root(1, 2), "a b");
    let docs = BTreeMap::from([
        (root(1, 1), "a"),
        (root(257, 1), "b"),
        (root(512, 291), "a b"),
        (root(1, 2), "a b"),
    ]);
    let baseline = FaultStore {
        inner,
        commits: 0,
        fail: None,
    };
    let mut complete = baseline.clone();
    build(&mut complete).unwrap();
    for point in 0..complete.commits {
        for after in [false, true] {
            let mut store = baseline.clone();
            store.fail = Some((point, after));
            assert!(
                build(&mut store).is_err(),
                "missing boundary {point}:{after}"
            );
            store.fail = None;
            check(&mut store, &docs, "a AND b");
            mutable::vacuum(&mut store, |_| Ok(false)).unwrap();
            check(&mut store, &docs, "a OR b");
            assert!(
                store
                    .read(0)
                    .unwrap()
                    .grouped_state()
                    .unwrap()
                    .journal
                    .is_none()
            );
            build(&mut store).unwrap();
            check(&mut store, &docs, "a AND b");
            free_pages(&mut store.inner);
        }
    }
}

#[test]
fn legacy_metadata_and_small_budget_fallback_remain_readable() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    assert_eq!(store.read(0).unwrap().bytes().len(), 4160);
    insert(&mut store, root(1, 1), "a b");
    assert_eq!(
        raw(&mut store, "a AND b", 8 << 20).unwrap(),
        vec![(root(1, 1), false)]
    );
    build(&mut store).unwrap();
    assert_eq!(store.read(0).unwrap().bytes().len(), 4232);
    assert_eq!(
        raw(&mut store, "a", 1 << 16).unwrap(),
        vec![(root(1, 1), false)]
    );
}

#[test]
fn maintenance_cutoff_skips_unchanged_and_retired_only_snapshots() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    assert!(grouped::needs_rebuild(&mut store).unwrap());
    build(&mut store).unwrap();
    assert!(!grouped::needs_rebuild(&mut store).unwrap());
    insert(&mut store, root(3, 1), "a");
    assert!(grouped::needs_rebuild(&mut store).unwrap());
    build(&mut store).unwrap();
    assert!(!grouped::needs_rebuild(&mut store).unwrap());
    mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
    assert!(!grouped::needs_rebuild(&mut store).unwrap());
    insert(&mut store, root(3, 1), "b");
    assert!(grouped::needs_rebuild(&mut store).unwrap());
    build(&mut store).unwrap();
    assert!(!grouped::needs_rebuild(&mut store).unwrap());
    assert!(raw(&mut store, "a AND b", 8 << 20).unwrap().is_empty());
    assert_eq!(
        raw(&mut store, "b", 8 << 20).unwrap(),
        vec![(root(3, 1), false)]
    );
}

#[test]
fn build_memory_floor_is_checked_before_sort_or_storage_changes() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, root(1, 1), "a b");
    let before = store.pages.clone();
    let required = grouped::build_memory(store.layout());
    let mut sort = Sort::default();
    assert_eq!(
        grouped::rebuild(&mut store, &mut sort, required - 1),
        Err(Error::Limit("group build scratch"))
    );
    assert!(sort.rows.is_empty());
    assert!(!sort.finished);
    assert_eq!(store.pages, before);
    assert_eq!(
        grouped::rebuild(&mut store, &mut sort, required)
            .unwrap()
            .documents,
        1
    );
}

#[test]
fn finished_sort_error_keeps_previous_snapshot_and_has_no_journal() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, root(1, 1), "a");
    build(&mut store).unwrap();
    insert(&mut store, root(1, 2), "a b");
    let before = store.pages.clone();
    store.events.clear();
    store.fail_at = Some(0);
    assert_eq!(build(&mut store), Err(Error::InvalidState));
    assert_eq!(store.events, vec![Stage::GroupSortReady]);
    assert_eq!(store.pages, before);
    store.fail_at = None;
    assert!(
        store
            .read(0)
            .unwrap()
            .grouped_state()
            .unwrap()
            .journal
            .is_none()
    );
    check(
        &mut store,
        &BTreeMap::from([(root(1, 1), "a"), (root(1, 2), "a b")]),
        "a AND b",
    );
    build(&mut store).unwrap();
    assert!(!grouped::needs_rebuild(&mut store).unwrap());
}

#[test]
fn grouped_scan_event_proves_selection_and_fallback_has_no_partial_output() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, root(1, 1), "a b");
    store.events.clear();
    raw(&mut store, "a", 8 << 20).unwrap();
    assert!(!store.events.contains(&Stage::GroupScan));
    build(&mut store).unwrap();
    for (query, budget, grouped) in [
        ("a", 8 << 20, false),
        ("a AND NOT b", 8 << 20, true),
        ("a*", 8 << 20, false),
        ("\"a b\"", 8 << 20, false),
        ("a", 1 << 16, false),
    ] {
        store.events.clear();
        raw(&mut store, query, budget).unwrap();
        assert_eq!(store.events.contains(&Stage::GroupScan), grouped);
    }
    store.events.clear();
    store.fail_at = Some(0);
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    let mut emitted = false;
    assert_eq!(
        grouped::scan_query(&mut store, &query, 8 << 20, |_, _| {
            emitted = true;
            Ok(())
        }),
        Err(Error::InvalidState)
    );
    assert!(!emitted);
}

#[derive(Default)]
struct Reads {
    total: usize,
    catalog: usize,
    liveness: usize,
    postings: usize,
}

struct Measured {
    inner: MemoryStore,
    reads: Reads,
}

impl PageStore for Measured {
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        let page = self.inner.read(block)?;
        self.reads.total += 1;
        if page.kind() == PageKind::Grouped {
            match page.group_identity()?.1 {
                GroupPageKind::Leaf | GroupPageKind::Branch => self.reads.catalog += 1,
                GroupPageKind::Liveness => self.reads.liveness += 1,
                GroupPageKind::Posting => self.reads.postings += 1,
                _ => {}
            }
        }
        Ok(page)
    }
    fn extend(&mut self) -> Result<u32> {
        self.inner.extend()
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        self.inner.commit(pages)
    }
    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
}

#[test]
fn selective_and_seeks_past_common_groups_in_both_operand_orders() {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    for group in 0..256 {
        insert(
            &mut inner,
            root(group * 256, 1),
            if group == 255 { "common rare" } else { "common" },
        );
    }
    build(&mut inner).unwrap();
    let mut store = Measured {
        inner,
        reads: Reads::default(),
    };
    for query in [
        "common AND rare",
        "rare AND common",
        "(common OR absent) AND rare",
        "rare AND (absent OR common)",
        "common AND NOT absent AND rare",
    ] {
        store.reads = Reads::default();
        assert_eq!(
            raw(&mut store, query, 8 << 20).unwrap(),
            vec![(root(255 * 256, 1), false)]
        );
        assert_eq!(store.reads.liveness, 1, "{query}");
        assert_eq!(store.reads.postings, 2, "{query}");
        assert!(
            store.reads.catalog < 32,
            "{query}: {} catalog reads",
            store.reads.catalog
        );
    }
}

#[test]
fn broad_scan_reuses_liveness_catalog_leaves() {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    for group in 0..130 {
        insert(&mut inner, root(group * 256, 1), "common");
    }
    build(&mut inner).unwrap();
    let mut store = Measured {
        inner,
        reads: Reads::default(),
    };
    assert_eq!(
        raw(&mut store, "common OR absent", 8 << 20).unwrap().len(),
        130
    );
    assert_eq!(store.reads.liveness, 130);
    assert_eq!(store.reads.postings, 130);
    assert!(
        store.reads.catalog < 32,
        "{} catalog reads",
        store.reads.catalog
    );
}

#[test]
fn sparse_path_uses_no_group_pages_or_extra_reads_and_covers_new_writes() {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    for group in 0..20 {
        insert(&mut inner, root(group * 256, 1), "rare");
    }
    build(&mut inner).unwrap();
    insert(&mut inner, root(1, 1), "rare");
    insert(&mut inner, root(2, 1), "unrelated");
    let mut store = Measured {
        inner,
        reads: Reads::default(),
    };
    let grouped = raw(&mut store, "rare", 8 << 20).unwrap();
    assert_eq!(grouped.len(), 21);
    assert!(grouped.iter().all(|(_, recheck)| !recheck));
    assert_eq!(
        store.reads.catalog + store.reads.liveness + store.reads.postings,
        0
    );
    let grouped_reads = store.reads.total;
    store.reads = Reads::default();
    let query = Query::parse("rare", QueryLimits::default()).unwrap();
    let mut legacy = Vec::new();
    mutable::scan_query_with_recheck(&mut store, &query, 8 << 20, |root, recheck| {
        legacy.push((root, recheck));
        Ok(())
    })
    .unwrap();
    assert_eq!(grouped, legacy);
    assert_eq!(grouped_reads, store.reads.total);
    assert!(raw(&mut store, "absent", 8 << 20).unwrap().is_empty());
}

#[test]
fn sparse_policy_stops_at_its_explicit_posting_bound() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    for offset in 1..=65 {
        insert(&mut store, root(0, offset), "term");
    }
    build(&mut store).unwrap();
    store.events.clear();
    assert_eq!(raw(&mut store, "term", 8 << 20).unwrap().len(), 65);
    assert!(store.events.contains(&Stage::GroupScan));
}

#[test]
fn boolean_group_bounds_preserve_holes_negation_and_last_heap_group() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let mut docs = BTreeMap::new();
    for group in 0..160 {
        let text = match group % 4 {
            0 => "a b",
            1 => "b c",
            2 => "a d",
            _ => "c d",
        };
        let tid = root(group * 512, 1);
        insert(&mut store, tid, text);
        docs.insert(tid, text);
    }
    insert(&mut store, root(u32::MAX - 1, 1), "b d");
    docs.insert(root(u32::MAX - 1, 1), "b d");
    build(&mut store).unwrap();
    for query in [
        "(a OR b) AND (c OR d)",
        "(c OR d) AND (a OR b)",
        "(a AND c) OR (b AND d)",
        "NOT (a OR d)",
        "(a OR NOT b) AND (c OR NOT d)",
        "NOT NOT b",
        "NOT absent",
        "a AND absent",
        "absent OR NOT a",
    ] {
        check(&mut store, &docs, query);
    }
}

#[test]
fn sparse_policy_covers_mutable_sealed_direct_and_retired_postings() {
    for (mode, kind) in [
        (None, PageKind::Postings),
        (Some(mutable::CompactMode::Copy), PageKind::SealedPostings),
        (Some(mutable::CompactMode::DirectTid), PageKind::DirectPostings),
    ] {
        let mut inner = MemoryStore::default();
        mutable::initialize(&mut inner).unwrap();
        for offset in 1..=64 {
            insert(&mut inner, root(0, offset), "term");
        }
        if let Some(mode) = mode {
            mutable::compact_with_mode(&mut inner, mode).unwrap();
        }
        assert!((1..inner.blocks().unwrap()).any(|block| inner.read(block).unwrap().kind() == kind));
        build(&mut inner).unwrap();
        mutable::vacuum(&mut inner, |tid| Ok(tid == root(0, 2))).unwrap();
        let mut store = Measured {
            inner,
            reads: Reads::default(),
        };
        let rows = raw(&mut store, "term", 8 << 20).unwrap();
        assert_eq!(rows.len(), 63, "{kind:?}");
        assert!(!rows.contains(&(root(0, 2), false)), "{kind:?}");
        assert!(rows.iter().all(|(_, recheck)| !recheck), "{kind:?}");
        assert_eq!(
            store.reads.catalog + store.reads.liveness + store.reads.postings,
            0,
            "{kind:?}"
        );
    }
}
