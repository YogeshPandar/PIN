use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::candidate::CandidatePlan;
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::mutable::page::{Page, PageKind};
use pin_core::mutable::{self, PageStore, document::PreparedDocument};
use pin_core::query::{Query, QueryLimits};

#[path = "support/mutable_store.rs"]
mod mutable_store;
use mutable_store::MemoryStore;

fn root(index: u32) -> RootTid {
    RootTid::new(index / 200 + 1, (index % 200 + 1) as u16, HeapLayout::new(291).unwrap()).unwrap()
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

impl PageStore for AppendStore {
    fn layout(&self) -> HeapLayout { self.inner.layout() }
    fn blocks(&mut self) -> Result<u32> { self.inner.blocks() }
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
    fn extend(&mut self) -> Result<u32> { self.inner.extend() }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> { self.inner.commit(pages) }
}

fn store(count: u32, end: u32, append_at: PageKind) -> AppendStore {
    let mut inner = MemoryStore::default();
    mutable::initialize(&mut inner).unwrap();
    let document = document();
    for index in 0..count {
        mutable::insert(&mut inner, root(index), &document).unwrap();
    }
    AppendStore { inner, append_at, first: count, end, owner_reads: 0, armed: true }
}

#[test]
fn owner_cache_refreshes_once_when_a_later_posting_outgrows_its_copy() {
    let query = Query::parse("a AND b", QueryLimits::default()).unwrap();
    for streaming in [false, true] {
        let mut store = store(2, 3, PageKind::Owners);
        let mut rows = Vec::new();
        if streaming {
            mutable::scan_query(&mut store, &query, 1 << 20, |root| { rows.push(root); Ok(()) }).unwrap();
        } else {
            let cover = CandidatePlan::build(&query, 1 << 20).unwrap();
            mutable::scan(&mut store, &cover, |root| { rows.push(root); Ok(()) }).unwrap();
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
    mutable::scan_query(&mut store, &query, 1 << 20, |root| { rows.push(root); Ok(()) }).unwrap();
    assert_eq!(rows, vec![root(0), root(1)]);
    assert!(!store.armed);
    rows.clear();
    mutable::scan_query(&mut store, &query, 1 << 20, |root| { rows.push(root); Ok(()) }).unwrap();
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
    assert!(matches!(result, Err(Error::Codec(_)) | Err(Error::InvalidState)));
    assert_eq!(store.owner_reads, 2);
}
