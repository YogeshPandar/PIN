use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::candidate::CandidatePlan;
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::grouped::{self, GroupSort, SortRecord};
use pin_core::mutable::page::{Page, PageKind};
use pin_core::mutable::{self, PageStore, document::PreparedDocument};
use pin_core::query::{Query, QueryLimits};

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

fn document() -> PreparedDocument {
    let analyzed = Analyzed::analyze("a b", AnalysisLimits::default()).unwrap();
    PreparedDocument::prepare(&analyzed, 1 << 20).unwrap()
}

struct AppendStore {
    inner: MemoryStore,
    append_at: PageKind,
    first: u32,
    end: u32,
    owner_reads: usize,
    armed: bool,
}

#[derive(Default)]
struct Sort {
    records: Vec<SortRecord>,
    position: usize,
}

impl GroupSort for Sort {
    fn put(&mut self, records: &[SortRecord]) -> Result<()> {
        self.records.extend_from_slice(records);
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.records.sort_unstable();
        Ok(())
    }
    fn read(&mut self, output: &mut [SortRecord]) -> Result<usize> {
        let count = output.len().min(self.records.len() - self.position);
        output[..count].copy_from_slice(&self.records[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

impl PageStore for AppendStore {
    fn frontier_anchors(&self) -> bool {
        self.inner.frontier_anchors
    }
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        let page = self.inner.read(block)?;
        self.owner_reads += usize::from(page.kind() == PageKind::Owners);
        if self.armed && page.kind() == self.append_at {
            self.armed = false;
            let document = document();
            for index in self.first..self.end {
                mutable::insert(&mut self.inner, root(index), &document)?;
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
    fn event(&mut self, stage: mutable::Stage) -> Result<()> {
        self.inner.event(stage)
    }
}

fn store(count: u32, end: u32, append_at: PageKind) -> AppendStore {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    let document = document();
    for index in 0..count {
        mutable::insert(&mut inner, root(index), &document).unwrap();
    }
    AppendStore {
        inner,
        append_at,
        first: count,
        end,
        owner_reads: 0,
        armed: true,
    }
}

#[test]
fn owner_cache_refreshes_once_when_a_later_posting_outgrows_its_copy() {
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    for streaming in [false, true] {
        let mut store = store(2, 3, PageKind::Owners);
        let mut rows = Vec::new();
        if streaming {
            mutable::scan_query(&mut store, &query, 1 << 20, |root| {
                rows.push(root);
                Ok(())
            })
            .unwrap();
        } else {
            let cover = CandidatePlan::build(&query, 1 << 20).unwrap();
            mutable::scan(&mut store, &cover, |root| {
                rows.push(root);
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(rows, vec![root(0), root(1), root(2)]);
        assert_eq!(store.owner_reads, 2);
        assert!(!store.armed);
    }
}

#[test]
fn captured_tail_stops_before_pages_appended_after_open() {
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    let mut store = store(2, 700, PageKind::Postings);
    let mut rows = Vec::new();
    mutable::scan_query(&mut store, &query, 1 << 20, |root| {
        rows.push(root);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, vec![root(0), root(1)]);
    assert!(!store.armed);
    rows.clear();
    mutable::scan_query(&mut store, &query, 1 << 20, |root| {
        rows.push(root);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, (0..700).map(root).collect::<Vec<_>>());
}

#[test]
fn refresh_does_not_hide_a_genuinely_missing_owner_slot() {
    let mut store = store(2, 2, PageKind::Zero);
    for block in 1..store.inner.pages.len() {
        let page = store.inner.read(block as u32).unwrap();
        if page.kind() == PageKind::Postings {
            store.inner.pages[block][28..30].copy_from_slice(&100u16.to_le_bytes());
        }
    }
    let query = Query::parse("a", QueryLimits::default()).unwrap();
    let result = mutable::scan_query(&mut store, &query, 1 << 20, |_| Ok(()));
    assert!(matches!(
        result,
        Err(Error::Codec(_)) | Err(Error::InvalidState)
    ));
    assert_eq!(store.owner_reads, 2);
}

#[test]
fn packed_head_survives_promotion_after_dictionary_capture() {
    let mut store = store(2, 3, PageKind::Dictionary);
    store.inner = MemoryStore {
        packed_postings: true,
        ..MemoryStore::default()
    };
    mutable::initialize(&mut store.inner).unwrap();
    for index in 0..2 {
        mutable::insert(&mut store.inner, root(index), &document()).unwrap();
    }
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    mutable::scan_query(&mut store, &query, 1 << 20, |root| {
        rows.push(root);
        Ok(())
    })
    .unwrap();
    assert!(!store.armed);
    assert_eq!(rows, vec![root(0), root(1)]);
    rows.clear();
    mutable::scan_query(&mut store, &query, 1 << 20, |root| {
        rows.push(root);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, vec![root(0), root(1), root(2)]);
}

#[test]
fn captured_grouped_anchor_falls_back_after_packed_promotion() {
    let mut store = store(2, 3, PageKind::Dictionary);
    store.inner = MemoryStore {
        packed_postings: true,
        frontier_anchors: true,
        ..MemoryStore::default()
    };
    mutable::initialize(&mut store.inner).unwrap();
    for index in 0..2 {
        mutable::insert(&mut store.inner, root(index), &document()).unwrap();
    }
    grouped::rebuild(&mut store.inner, &mut Sort::default(), 8 << 20).unwrap();
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    grouped::scan_query(&mut store, &query, 8 << 20, |root, _| {
        rows.push(root);
        Ok(())
    })
    .unwrap();
    assert!(!store.armed);
    assert_eq!(rows, vec![root(0), root(1)]);
    assert!(
        store
            .inner
            .events
            .contains(&mutable::Stage::FrontierInvalidated)
    );
}
