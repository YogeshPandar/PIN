#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::candidate::CandidatePlan;
use pin_core::error::Result;
use pin_core::identity::RootTid;
use pin_core::mutable::document::PreparedDocument;
use pin_core::mutable::grouped::{self, GroupSort, SortRecord};
use pin_core::mutable::page::{Page, PageKind};
use pin_core::mutable::{self, PageStore};
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
        let count = out.len().min(self.rows.len() - self.position);
        out[..count].copy_from_slice(&self.rows[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

fn root(store: &MemoryStore, index: u32) -> RootTid {
    RootTid::new(index / 200 + 1, (index % 200 + 1) as u16, store.layout()).unwrap()
}

fn insert(store: &mut MemoryStore, index: u32, text: &str) {
    let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
    let prepared = PreparedDocument::prepare(&analyzed, 32 << 20).unwrap();
    let tid = root(store, index);
    mutable::insert(store, tid, &prepared).unwrap();
}

fn rows(store: &mut MemoryStore, source: &str) -> BTreeSet<RootTid> {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let plan = CandidatePlan::build(&query, 1 << 20).unwrap();
    let mut result = BTreeSet::new();
    mutable::scan(store, &plan, |root| {
        result.insert(root);
        Ok(())
    })
    .unwrap();
    result
}

#[test]
fn two_document_vocabulary_has_no_dedicated_posting_pages() {
    let mut store = MemoryStore {
        packed_postings: true,
        ..MemoryStore::default()
    };
    mutable::initialize(&mut store).unwrap();
    for term in 0..1000 {
        let word = format!("word{term}");
        insert(&mut store, term * 2, &word);
        insert(&mut store, term * 2 + 1, &word);
    }
    let mut packed = 0;
    let mut postings = 0;
    for block in 0..store.blocks().unwrap() {
        let page = store.read(block).unwrap();
        page.validate(store.layout()).unwrap();
        packed += usize::from(page.packed_dictionary_format());
        postings += usize::from(page.kind() == PageKind::Postings);
    }
    assert!(packed > 0);
    assert_eq!(postings, 0);
    for term in [0, 1, 499, 999] {
        let word = format!("word{term}");
        assert_eq!(
            rows(&mut store, &word),
            BTreeSet::from([root(&store, term * 2), root(&store, term * 2 + 1)])
        );
    }
}

#[test]
fn third_owner_promotes_and_dead_second_is_reusable() {
    let mut store = MemoryStore {
        packed_postings: true,
        ..MemoryStore::default()
    };
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, 0, "alpha");
    insert(&mut store, 1, "alpha");
    assert_eq!(
        rows(&mut store, "alpha"),
        BTreeSet::from([root(&store, 0), root(&store, 1)])
    );
    let dead = root(&store, 1);
    mutable::vacuum(&mut store, |tid| Ok(tid == dead)).unwrap();
    mutable::compact(&mut store).unwrap();
    assert_eq!(rows(&mut store, "alpha"), BTreeSet::from([root(&store, 0)]));
    insert(&mut store, 2, "alpha");
    insert(&mut store, 3, "alpha");
    assert_eq!(
        rows(&mut store, "alpha"),
        BTreeSet::from([root(&store, 0), root(&store, 2), root(&store, 3)])
    );
    mutable::compact(&mut store).unwrap();
    assert_eq!(
        rows(&mut store, "alpha"),
        BTreeSet::from([root(&store, 0), root(&store, 2), root(&store, 3)])
    );
}

#[test]
fn grouped_build_reads_inline_second_and_promoted_suffix() {
    let mut store = MemoryStore {
        packed_postings: true,
        frontier_anchors: true,
        ..MemoryStore::default()
    };
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, 0, "alpha beta");
    insert(&mut store, 1, "alpha beta");
    grouped::rebuild(&mut store, &mut Sort::default(), 8 << 20).unwrap();
    assert!(
        store
            .read(0)
            .unwrap()
            .grouped_state()
            .unwrap()
            .active
            .unwrap()
            .frontier_valid
    );
    let query = Query::parse("alpha AND beta", QueryLimits::default()).unwrap();
    let mut first = BTreeSet::new();
    grouped::scan_query(&mut store, &query, 8 << 20, |root, _| {
        first.insert(root);
        Ok(())
    })
    .unwrap();
    assert_eq!(first, BTreeSet::from([root(&store, 0), root(&store, 1)]));
    insert(&mut store, 2, "alpha beta");
    assert!(
        !store
            .read(0)
            .unwrap()
            .grouped_state()
            .unwrap()
            .active
            .unwrap()
            .frontier_valid
    );
    let mut after = BTreeSet::new();
    grouped::scan_query(&mut store, &query, 8 << 20, |root, _| {
        after.insert(root);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        after,
        BTreeSet::from([root(&store, 0), root(&store, 1), root(&store, 2)])
    );
}

#[test]
fn malformed_packed_states_fail_before_a_scan() {
    let mut store = MemoryStore {
        packed_postings: true,
        ..MemoryStore::default()
    };
    mutable::initialize(&mut store).unwrap();
    insert(&mut store, 0, "alpha");
    insert(&mut store, 1, "alpha");
    let (block, offset) = (1..store.blocks().unwrap())
        .find_map(|block| {
            let page = store.read(block).unwrap();
            if page.packed_dictionary_format() {
                page.terms().unwrap().find_map(|term| {
                    let term = term.unwrap();
                    (term.term == "alpha").then_some((block, usize::from(term.reference.offset)))
                })
            } else {
                None
            }
        })
        .unwrap();
    for (position, value) in [(7, 2), (offset + 28, 3), (offset + 32 + 5 + 6, 1)] {
        let mut corrupt = store.pages[block as usize].clone();
        corrupt[position] = value;
        let result = Page::read_with(block, |out| {
            out[..corrupt.len()].copy_from_slice(&corrupt);
            Ok(corrupt.len())
        });
        assert!(result.is_err() || result.unwrap().validate(store.layout()).is_err());
    }
}
