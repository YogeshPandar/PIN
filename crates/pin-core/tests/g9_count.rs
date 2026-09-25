#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::grouped::{self, GroupSort, SortRecord};
use pin_core::mutable::{self, PageStore, document::PreparedDocument};
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

#[derive(Default)]
struct PageOracle {
    pages: BTreeMap<u32, BTreeSet<RootTid>>,
    roots: Vec<(RootTid, bool)>,
    decoded: u64,
    fail: bool,
}

impl grouped::GroupSink for PageOracle {
    fn root(&mut self, root: RootTid, recheck: bool) -> Result<()> {
        self.roots.push((root, recheck));
        Ok(())
    }
    fn page(&mut self, block: u32, offsets: &[u64; 8], layout: HeapLayout) -> Result<()> {
        if self.fail {
            return Err(Error::InvalidState);
        }
        let roots = self.pages.entry(block).or_default();
        // enumerate the independent physical domain instead of copying the bit iterator.
        for offset in 1..=291u16 {
            let bit = usize::from(offset - 1);
            if offsets[bit / 64] & (1 << (bit % 64)) != 0 {
                assert!(roots.insert(RootTid::new(block, offset, layout).unwrap()));
            }
        }
        assert_eq!(roots.len() as u32, offsets.iter().map(|w| w.count_ones()).sum::<u32>());
        Ok(())
    }
    fn work(&mut self, stats: &pin_core::grouped::QueryStats) -> Result<()> {
        self.decoded += u64::from(stats.term_bytes);
        Ok(())
    }
}

#[test]
fn page_sink_matches_independent_rows_and_never_certifies_frontier_roots() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let mut docs = BTreeMap::new();
    for block in [0, 1, 256, u32::MAX - 1] {
        for offset in [1, 2, 64, 65, 128, 129, 256, 291] {
            let text = if offset % 2 == 0 { "a b" } else { "a c" };
            insert(&mut store, root(block, offset), text);
            docs.insert(root(block, offset), text);
        }
    }
    build(&mut store).unwrap();
    let retired = root(0, 2);
    mutable::vacuum(&mut store, |tid| Ok(tid == retired)).unwrap();
    // the reused root belongs only to the newer owner, not the retired a+b owner.
    insert(&mut store, retired, "b c");
    docs.insert(retired, "b c");
    let delta = root(1, 3);
    insert(&mut store, delta, "a b c");
    docs.insert(delta, "a b c");
    for source in ["a AND b", "a OR b", "NOT a", "NOT (b AND c)", "a AND absent"] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        assert!(grouped::supports_query(&query));
        let mut sink = PageOracle::default();
        let count = grouped::scan_into(&mut store, &query, 8 << 20, &mut sink).unwrap();
        let mut actual = BTreeSet::new();
        for roots in sink.pages.values() {
            for &tid in roots {
                assert!(actual.insert(tid));
                assert_ne!(tid, retired);
                assert_ne!(tid, delta);
            }
        }
        for &(tid, recheck) in &sink.roots {
            assert!(!recheck);
            assert!(actual.insert(tid));
        }
        let mut expected = BTreeSet::new();
        for (&tid, text) in &docs {
            let doc = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
            if oracle::matches(&doc, &query, 1 << 20, 1 << 20).unwrap() {
                expected.insert(tid);
            }
        }
        assert_eq!(actual, expected, "{source}");
        assert_eq!(count, actual.len() as u64);
        // model mixed VM pages with a visibility oracle independent of postings.
        for visible_mod in 1..=4 {
            let visible = |tid: RootTid| tid.block() % visible_mod == 0 || tid.offset() % 2 == 0;
            let from_pages = sink.pages.values().flatten().filter(|&&tid| visible(tid)).count();
            let from_roots = sink.roots.iter().filter(|&&(tid, _)| visible(tid)).count();
            assert_eq!(from_pages + from_roots, expected.iter().filter(|&&tid| visible(tid)).count());
        }
    }
    assert!(!raw(&mut store, "a AND b", 8 << 20).unwrap().contains(&(retired, false)));
}

#[test]
fn page_sink_fallbacks_never_claim_a_snapshot_and_callback_errors_propagate() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    for offset in 1..=65 {
        insert(&mut store, root(0, offset), "a b");
    }
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    let mut sink = PageOracle::default();
    assert_eq!(grouped::scan_into(&mut store, &query, 8 << 20, &mut sink).unwrap(), 65);
    assert!(sink.pages.is_empty());
    build(&mut store).unwrap();
    for (source, budget) in [("a AND b", 1), ("a*", 8 << 20), ("\"a b\"", 8 << 20)] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        let mut sink = PageOracle::default();
        // a tiny budget may fail closed; it must never label roots as snapshot pages.
        let result = grouped::scan_into(&mut store, &query, budget, &mut sink);
        assert!(sink.pages.is_empty());
        if budget > 1 {
            assert!(result.is_ok());
            assert!(!grouped::supports_query(&query));
        }
    }
    let mut sink = PageOracle { fail: true, ..PageOracle::default() };
    assert!(grouped::scan_into(&mut store, &query, 8 << 20, &mut sink).is_err());
    // a caller discards partial output after error; another scan remains usable.
    assert_eq!(raw(&mut store, "a AND b", 8 << 20).unwrap().len(), 65);
}
