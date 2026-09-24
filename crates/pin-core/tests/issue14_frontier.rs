#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::grouped::{self, GroupSort, SortRecord};
use pin_core::mutable::page::{Page, PageKind};
use pin_core::mutable::{self, PageStore, Stage, document::PreparedDocument};
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeMap;

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
        let count = out.len().min(self.rows.len() - self.position).min(13);
        out[..count].copy_from_slice(&self.rows[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

fn tid(n: u32) -> RootTid {
    RootTid::new(n / 200, (n % 200 + 1) as u16, HeapLayout::new(291).unwrap()).unwrap()
}

fn insert(store: &mut impl PageStore, n: u32, text: &str) -> Result<()> {
    let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
    let prepared = PreparedDocument::prepare(&analyzed, 8 << 20).unwrap();
    mutable::insert(store, tid(n), &prepared).map(|_| ())
}

fn build(store: &mut impl PageStore) {
    grouped::rebuild(store, &mut Sort::default(), 8 << 20).unwrap();
}

fn rows(store: &mut impl PageStore, source: &str, budget: usize) -> Result<Vec<(RootTid, bool)>> {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    let count = grouped::scan_query(store, &query, budget, |root, recheck| {
        rows.push((root, recheck));
        Ok(())
    })?;
    assert_eq!(count, rows.len() as u64);
    rows.sort_unstable_by_key(|(root, _)| *root);
    Ok(rows)
}

fn exact(store: &mut impl PageStore, docs: &BTreeMap<u32, &str>, source: &str) {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let expected: Vec<_> = docs
        .iter()
        .filter_map(|(&n, text)| {
            let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
            pin_core::oracle::matches(&document, &query, 1 << 20, 1 << 20)
                .unwrap()
                .then_some((tid(n), false))
        })
        .collect();
    assert_eq!(rows(store, source, 8 << 20).unwrap(), expected, "{source}");
}

#[derive(Default)]
struct Measured {
    inner: MemoryStore,
    reads: usize,
    owners: usize,
    postings: usize,
    fail_read: Option<usize>,
    interrupts: usize,
    fail_interrupt: Option<usize>,
}

impl PageStore for Measured {
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        self.reads += 1;
        if self.fail_read == Some(self.reads) {
            return Err(Error::InvalidState);
        }
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
    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
    fn interrupt(&mut self) -> Result<()> {
        self.interrupts += 1;
        if self.fail_interrupt == Some(self.interrupts) {
            Err(Error::InvalidState)
        } else {
            Ok(())
        }
    }
}

#[test]
fn growing_suffix_boolean_truth_tables_are_exact_including_complements() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let texts = ["", "a", "b", "c", "a b", "a c", "b c", "a b c", "newterm"];
    let mut docs = BTreeMap::new();
    for (n, text) in texts.iter().enumerate() {
        insert(&mut store, n as u32, text).unwrap();
        docs.insert(n as u32, *text);
    }
    build(&mut store);
    for round in 1..=2 {
        for (n, text) in texts.iter().rev().enumerate() {
            let n = round * 200 + n as u32;
            insert(&mut store, n, text).unwrap();
            docs.insert(n, *text);
        }
        for left in ["a", "missing", "NOT b", "a OR c", "NOT (a AND b)", "newterm"] {
            for right in ["b", "missing", "NOT a", "b AND c", "NOT (b OR c)", "newterm"] {
                for op in ["AND", "OR"] {
                    let query = format!("({left}) {op} ({right})");
                    exact(&mut store, &docs, &query);
                    exact(&mut store, &docs, &format!("NOT ({query})"));
                }
            }
        }
    }
    for query in ["a AND a", "NOT NOT a", "NOT missing", "", "a OR NOT a", "a AND NOT a"] {
        exact(&mut store, &docs, query);
    }
}

#[test]
fn unrelated_writes_neither_emit_candidates_nor_scan_the_owner_delta() {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    for n in 0..2500 {
        insert(&mut inner, n, if n % 125 == 0 { "alpha rare" } else { "alpha beta" }).unwrap();
    }
    build(&mut inner);
    let mut store = Measured { inner, ..Measured::default() };
    let expected = rows(&mut store, "alpha AND rare", 8 << 20).unwrap();
    assert_eq!(expected.len(), 20);
    let fresh_reads = store.reads;
    for (start, end) in [(2500, 3500), (3500, 8500)] {
        for n in start..end {
            insert(&mut store.inner, n, "unrelated filler").unwrap();
        }
        store.reads = 0;
        store.owners = 0;
        assert_eq!(rows(&mut store, "alpha AND rare", 8 << 20).unwrap(), expected);
        assert_eq!(store.owners, 1, "must only inspect the cutoff owner page");
        assert!(store.reads <= fresh_reads + 2, "two term-tail probes, not a delta walk");
        assert!(rows(&mut store, "missing AND alpha", 8 << 20).unwrap().is_empty());
    }
}

#[test]
fn a_straddling_tail_avoids_old_posting_pages() {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    for n in 0..2500 {
        insert(&mut inner, n, "alpha").unwrap();
    }
    build(&mut inner);
    for n in 2500..2540 {
        insert(&mut inner, n, "alpha newterm").unwrap();
    }
    let mut store = Measured { inner, ..Measured::default() };
    let expected: Vec<_> = (2500..2540).map(|n| (tid(n), false)).collect();
    assert_eq!(rows(&mut store, "alpha AND newterm", 8 << 20).unwrap(), expected);
    assert_eq!(store.postings, 2, "one captured tail per non-inline term");
}

#[test]
fn wholly_new_tail_does_not_hide_its_newer_predecessors() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    for n in 0..100 {
        insert(&mut store, n, "alpha").unwrap();
    }
    build(&mut store);
    for n in 100..1800 {
        insert(&mut store, n, "alpha newterm").unwrap();
    }
    let expected: Vec<_> = (100..1800).map(|n| (tid(n), false)).collect();
    for query in ["alpha AND newterm", "newterm AND alpha", "newterm AND NOT absent"] {
        assert_eq!(rows(&mut store, query, 8 << 20).unwrap(), expected, "{query}");
    }
}

#[test]
fn suffixes_survive_compaction_retirement_and_reused_heap_coordinates() {
    for mode in [mutable::CompactMode::Copy, mutable::CompactMode::DirectTid] {
        let mut store = MemoryStore::default();
        mutable::initialize(&mut store).unwrap();
        let mut docs = BTreeMap::new();
        for n in 0..100 {
            insert(&mut store, n, "alpha beta").unwrap();
            docs.insert(n, "alpha beta");
        }
        mutable::compact_with_mode(&mut store, mode).unwrap();
        build(&mut store);
        mutable::vacuum(&mut store, |root| Ok(root == tid(0))).unwrap();
        insert(&mut store, 0, "beta newterm").unwrap();
        docs.insert(0, "beta newterm");
        for n in 100..125 {
            insert(&mut store, n, "alpha newterm").unwrap();
            docs.insert(n, "alpha newterm");
        }
        // compaction may replace term chains without rebuilding the grouped generation.
        mutable::compact_with_mode(&mut store, mode).unwrap();
        for query in ["alpha AND beta", "alpha AND NOT beta", "NOT alpha", "newterm OR alpha"] {
            exact(&mut store, &docs, query);
        }
    }
}

#[test]
fn empty_snapshot_and_new_dictionary_terms_are_searchable() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    build(&mut store);
    let docs = BTreeMap::from([(0, ""), (1, "a"), (2, "b"), (3, "a b")]);
    for (&n, text) in &docs {
        insert(&mut store, n, text).unwrap();
    }
    for query in ["a AND b", "a OR b", "NOT a", "NOT missing", "a AND NOT b"] {
        exact(&mut store, &docs, query);
    }
}

#[test]
fn incomplete_insert_publication_never_becomes_an_exact_suffix_hit() {
    let mut base = MemoryStore::default();
    mutable::initialize(&mut base).unwrap();
    insert(&mut base, 0, "alpha").unwrap();
    build(&mut base);
    let mut complete = base.clone();
    complete.events.clear();
    insert(&mut complete, 1, "alpha beta newterm").unwrap();
    for point in 0..complete.events.len() {
        let mut store = base.clone();
        store.events.clear();
        store.fail_at = Some(point);
        assert!(insert(&mut store, 1, "alpha beta newterm").is_err());
        store.fail_at = None;
        // publication can succeed just before the injected post-commit event error.
        let accepted = rows(&mut store, "newterm", 8 << 20).unwrap();
        let mut docs = BTreeMap::from([(0, "alpha")]);
        if !accepted.is_empty() {
            docs.insert(1, "alpha beta newterm");
        }
        for query in ["alpha AND beta", "alpha OR beta", "NOT beta", "NOT missing"] {
            exact(&mut store, &docs, query);
        }
    }
}

#[test]
fn read_errors_and_cancellation_leave_storage_unchanged_and_retryable() {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    insert(&mut inner, 0, "alpha beta").unwrap();
    build(&mut inner);
    for n in 1..100 {
        insert(&mut inner, n, if n % 2 == 0 { "alpha" } else { "beta" }).unwrap();
    }
    let mut reference = Measured { inner: inner.clone(), ..Measured::default() };
    let expected = rows(&mut reference, "alpha AND NOT beta", 8 << 20).unwrap();
    for point in 1..=reference.reads {
        let mut store = Measured {
            inner: inner.clone(), fail_read: Some(point), ..Measured::default()
        };
        assert!(rows(&mut store, "alpha AND NOT beta", 8 << 20).is_err());
        assert_eq!(store.inner.pages, inner.pages);
        store.fail_read = None;
        assert_eq!(rows(&mut store, "alpha AND NOT beta", 8 << 20).unwrap(), expected);
    }
    for point in 1..=reference.interrupts {
        let mut store = Measured {
            inner: inner.clone(), fail_interrupt: Some(point), ..Measured::default()
        };
        assert!(rows(&mut store, "alpha AND NOT beta", 8 << 20).is_err());
        assert_eq!(store.inner.pages, inner.pages);
    }
}
