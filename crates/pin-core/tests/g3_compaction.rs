#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::candidate::CandidatePlan;
use pin_core::codec::bytes::{Reader, Writer};
use pin_core::error::{Error, Result};
use pin_core::identity::{HeapLayout, Incarnation, RootTid};
use pin_core::mutable::document::PreparedDocument;
use pin_core::mutable::page::{
    NO_BLOCK, OwnerRef, Page, PageKind, RewriteJournal, RewritePhase, SealedBuilder, TermRef,
    bucket_for,
};
use pin_core::mutable::{self, PageStore, Stage};
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeSet;

fn prepared(text: &str) -> PreparedDocument {
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

fn scan(store: &mut MemoryStore, text: &str) -> Result<Vec<RootTid>> {
    let query = Query::parse(text, QueryLimits::default()).unwrap();
    let plan = CandidatePlan::build(&query, 1 << 20).unwrap();
    let mut rows = Vec::new();
    mutable::scan(store, &plan, |root| {
        rows.push(root);
        Ok(())
    })?;
    Ok(rows)
}

fn candidates(store: &mut MemoryStore, text: &str) -> BTreeSet<RootTid> {
    scan(store, text).unwrap().into_iter().collect()
}

fn seed(count: u32, text: &str) -> MemoryStore {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let document = prepared(text);
    for index in 0..count {
        mutable::insert(&mut store, root(index), &document).unwrap();
    }
    store.events.clear();
    store
}

fn free_list(store: &mut MemoryStore) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let mut block = store.read(0).unwrap().free_head().unwrap();
    while block != NO_BLOCK {
        assert!(seen.insert(block), "free-list cycle or duplicate ownership");
        let page = store.read(block).unwrap();
        assert_eq!(page.kind(), PageKind::Free);
        block = page.next().unwrap();
    }
    for block in 1..store.blocks().unwrap() {
        assert_eq!(
            seen.contains(&block),
            store.read(block).unwrap().kind() == PageKind::Free
        );
    }
    seen
}

fn owner(index: u32) -> OwnerRef {
    OwnerRef {
        page: index / 200 + 1,
        slot: (index % 200) as u16,
        incarnation: Incarnation::new(u64::from(index) + 1).unwrap(),
    }
}

#[test]
fn u64_varints_cover_boundaries_and_preserve_failed_cursors() {
    let mut values = vec![0, 1, u64::MAX];
    for bit in 1..64 {
        let value = 1u64 << bit;
        values.extend([value - 1, value]);
    }
    for value in values {
        let mut bytes = [0u8; 10];
        let mut writer = Writer::new(&mut bytes);
        writer.var_u64(value).unwrap();
        let len = writer.len();
        let mut reader = Reader::new(&bytes[..len]);
        assert_eq!(reader.var_u64().unwrap(), value);
        reader.finish().unwrap();
        for truncated in 0..len {
            let mut reader = Reader::new(&bytes[..truncated]);
            assert!(reader.var_u64().is_err());
            assert_eq!(reader.offset(), 0);
        }
    }
    for bytes in [
        vec![0x80, 0],
        vec![0x81, 0],
        vec![0xff; 10],
        [vec![0xff; 9], vec![2]].concat(),
        vec![0x80; 11],
    ] {
        let mut reader = Reader::new(&bytes);
        assert!(reader.var_u64().is_err());
        assert_eq!(reader.offset(), 0);
    }
}

#[test]
fn sealed_pages_round_trip_without_a_page_sized_reference_array() {
    let term = TermRef {
        page: 99,
        offset: 16,
    };
    let mut builder = SealedBuilder::new(100, term).unwrap();
    let mut expected = Vec::new();
    for index in 0..10_000 {
        if !builder.push(owner(index)).unwrap() {
            break;
        }
        expected.push(owner(index));
    }
    assert!(expected.len() > 2500);
    let page = builder.finish().unwrap();
    page.validate(HeapLayout::new(291).unwrap()).unwrap();
    assert_eq!(page.kind(), PageKind::SealedPostings);
    assert_eq!(page.posting_term().unwrap(), term);
    assert_eq!(
        page.posting_refs()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap(),
        expected
    );
    assert!(page.bytes().len() < expected.len() * 4);
    let copy = Page::read_with(100, |out| {
        out[..page.bytes().len()].copy_from_slice(page.bytes());
        Ok(page.bytes().len())
    })
    .unwrap();
    copy.validate(HeapLayout::new(291).unwrap()).unwrap();
    assert_eq!(copy.bytes(), page.bytes());
}

#[test]
fn sealed_codec_rejects_invalid_order_counts_trailing_bytes_and_old_version() {
    let term = TermRef {
        page: 99,
        offset: 16,
    };
    assert!(SealedBuilder::new(100, term).unwrap().finish().is_err());
    let mut builder = SealedBuilder::new(100, term).unwrap();
    builder.push(owner(0)).unwrap();
    assert!(builder.push(owner(0)).is_err());
    builder.push(owner(1)).unwrap();
    let page = builder.finish().unwrap();
    let valid = page.bytes().to_vec();
    assert_eq!(&valid[24..], &[1, 0, 1, 0, 1, 1]);
    let mut cases = Vec::new();
    for offset in [4, 22, 26, 28, 29] {
        let mut bytes = valid.clone();
        bytes[offset] = if offset == 4 { 1 } else { 0 };
        cases.push(bytes);
    }
    let mut trailing = valid.clone();
    trailing.push(0);
    cases.push(trailing);
    let mut too_many = valid.clone();
    too_many[22] = 3;
    cases.push(too_many);
    let mut noncanonical = valid.clone();
    noncanonical.splice(24..25, [0x81, 0]);
    cases.push(noncanonical);
    for end in 0..valid.len() {
        cases.push(valid[..end].to_vec());
    }
    for bytes in cases {
        let parsed = Page::read_with(100, |out| {
            out[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        });
        // zero bytes represent an allocation orphan, never a sealed posting page.
        assert!(
            parsed.is_err()
                || parsed.as_ref().unwrap().kind() == PageKind::Zero
                || parsed
                    .unwrap()
                    .validate(HeapLayout::new(291).unwrap())
                    .is_err()
        );
    }
    let mut extremes = SealedBuilder::new(100, term).unwrap();
    let maximum = OwnerRef {
        page: u32::MAX - 1,
        slot: 202,
        incarnation: Incarnation::new(u64::MAX).unwrap(),
    };
    extremes.push(maximum).unwrap();
    let page = extremes.finish().unwrap();
    page.validate(HeapLayout::new(291).unwrap()).unwrap();
    assert_eq!(
        page.posting_refs().unwrap().next().unwrap().unwrap(),
        maximum
    );
}

#[test]
fn multi_page_segments_recycle_blocks_and_accept_a_mutable_tail() {
    let mut store = seed(6000, "alpha beta alpha");
    let expected = candidates(&mut store, "alpha");
    let owner_images: Vec<_> = (1..store.blocks().unwrap())
        .filter_map(|block| {
            let page = store.read(block).unwrap();
            (page.kind() == PageKind::Owners).then(|| (block, page.bytes().to_vec()))
        })
        .collect();
    let stats = mutable::compact(&mut store).unwrap();
    assert_eq!(stats.rewritten_terms, 2);
    assert!(stats.written_pages >= 4);
    assert!(stats.reclaimed_pages > stats.written_pages * 2);
    assert!(stats.reused_pages > 0);
    assert_eq!(stats.removed_postings, 0);
    let mut descending = false;
    for block in 1..store.blocks().unwrap() {
        let page = store.read(block).unwrap();
        if page.kind() == PageKind::SealedPostings {
            descending |= page.next().unwrap() < block;
        }
    }
    assert!(
        descending,
        "recycled chains must not depend on ascending block numbers"
    );
    assert_eq!(candidates(&mut store, "alpha"), expected);
    assert_eq!(scan(&mut store, "alpha").unwrap().len(), expected.len());
    for (block, bytes) in owner_images {
        assert_eq!(store.read(block).unwrap().bytes(), bytes);
    }
    for source in [
        "beta",
        "alpha OR beta",
        "alpha AND beta",
        "NOT missing",
        "al*",
        "\"alpha beta\"",
    ] {
        assert_eq!(candidates(&mut store, source), expected);
    }
    let before = store.pages.clone();
    assert_eq!(
        mutable::compact(&mut store).unwrap(),
        mutable::CompactStats::default()
    );
    assert_eq!(before, store.pages);
    mutable::insert(&mut store, root(6000), &prepared("alpha beta")).unwrap();
    assert_eq!(scan(&mut store, "alpha").unwrap().len(), 6001);
    mutable::compact(&mut store).unwrap();
    assert_eq!(scan(&mut store, "alpha").unwrap().len(), 6001);
    assert_eq!(
        candidates(&mut store, "alpha"),
        (0..6001).map(root).collect()
    );
    free_list(&mut store);
}

#[test]
fn compaction_preserves_terms_sharing_one_dictionary_page() {
    assert_eq!(bucket_for("aav"), bucket_for("ala"));
    let mut store = seed(1300, "aav ala");
    let expected: BTreeSet<_> = (0..1300).map(root).collect();

    let meta = store.read(0).unwrap();
    let (dictionary_block, _) = meta.bucket(bucket_for("aav")).unwrap();
    let dictionary = store.read(dictionary_block).unwrap();
    let entries = dictionary
        .terms()
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(entries.iter().any(|entry| entry.term == "aav"));
    assert!(entries.iter().any(|entry| entry.term == "ala"));

    // each replacement must preserve sibling metadata in the shared dictionary image.
    let stats = mutable::compact(&mut store).unwrap();
    assert_eq!(stats.rewritten_terms, 2);
    assert_eq!(candidates(&mut store, "aav"), expected);
    assert_eq!(candidates(&mut store, "ala"), expected);

    mutable::insert(&mut store, root(1300), &prepared("aav")).unwrap();
    let stats = mutable::compact(&mut store).unwrap();
    assert_eq!(stats.rewritten_terms, 1);

    let mut aav = expected.clone();
    aav.insert(root(1300));
    assert_eq!(candidates(&mut store, "aav"), aav);
    assert_eq!(candidates(&mut store, "ala"), expected);
    free_list(&mut store);
}

#[test]
fn vacuum_and_reused_heap_slots_cannot_resurrect_sealed_postings() {
    let mut store = seed(700, "alpha beta");
    mutable::compact(&mut store).unwrap();
    mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
    let stats = mutable::compact(&mut store).unwrap();
    assert_eq!(stats.written_pages, 0);
    assert_eq!(stats.removed_postings, 1398);
    assert!(candidates(&mut store, "alpha").is_empty());
    mutable::insert(&mut store, root(0), &prepared("gamma")).unwrap();
    assert!(candidates(&mut store, "alpha").is_empty());
    assert_eq!(candidates(&mut store, "gamma"), BTreeSet::from([root(0)]));
    mutable::insert(&mut store, root(1), &prepared("alpha")).unwrap();
    mutable::compact(&mut store).unwrap();
    assert_eq!(candidates(&mut store, "alpha"), BTreeSet::from([root(1)]));
    assert_eq!(
        candidates(&mut store, "NOT absent"),
        BTreeSet::from([root(0), root(1)])
    );
    free_list(&mut store);
}

#[test]
fn every_durable_compaction_boundary_preserves_coverage_and_recovers() {
    let baseline = seed(3000, "alpha beta gamma");
    let mut complete = baseline.clone();
    mutable::compact(&mut complete).unwrap();
    for stage in [
        Stage::SegmentStored,
        Stage::ReplacementPublished,
        Stage::SegmentReclaimed,
    ] {
        assert!(complete.events.contains(&stage));
    }
    let expected: BTreeSet<_> = (0..3000).map(root).collect();
    for point in 0..complete.events.len() {
        let mut crashed = baseline.clone();
        crashed.fail_at = Some(point);
        assert!(mutable::compact(&mut crashed).is_err());
        crashed.fail_at = None;
        for source in ["alpha", "beta", "gamma"] {
            assert_eq!(candidates(&mut crashed, source), expected, "point {point}");
        }
        mutable::recover_compaction(&mut crashed).unwrap();
        assert_eq!(mutable::recover_compaction(&mut crashed).unwrap(), 0);
        mutable::vacuum(&mut crashed, |_| Ok(false)).unwrap();
        mutable::compact(&mut crashed).unwrap();
        assert_eq!(candidates(&mut crashed, "alpha"), expected);
        assert!(
            crashed
                .read(0)
                .unwrap()
                .rewrite_journal()
                .unwrap()
                .is_none()
        );
        free_list(&mut crashed);
    }
}

struct CommitFault {
    inner: MemoryStore,
    stop: Option<(usize, bool)>,
    commits: usize,
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
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        let current = self.commits;
        self.commits += 1;
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
fn wal_batch_failures_before_and_after_persistence_are_atomic() {
    let baseline = seed(700, "alpha beta");
    let mut complete = CommitFault {
        inner: baseline.clone(),
        stop: None,
        commits: 0,
    };
    mutable::compact(&mut complete).unwrap();
    for point in 0..complete.commits {
        for after in [false, true] {
            let mut fault = CommitFault {
                inner: baseline.clone(),
                stop: Some((point, after)),
                commits: 0,
            };
            assert!(mutable::compact(&mut fault).is_err());
            fault.stop = None;
            mutable::recover_compaction(&mut fault).unwrap();
            mutable::vacuum(&mut fault, |_| Ok(false)).unwrap();
            mutable::compact(&mut fault).unwrap();
            assert_eq!(scan(&mut fault.inner, "alpha").unwrap().len(), 700);
            assert_eq!(scan(&mut fault.inner, "beta").unwrap().len(), 700);
            free_list(&mut fault.inner);
        }
    }
}

#[test]
fn corrupted_journal_cannot_reclaim_the_active_chain() {
    let mut store = seed(700, "alpha");
    let mut meta = store.read(0).unwrap();
    let (block, _) = meta.bucket(bucket_for("alpha")).unwrap();
    let dictionary = store.read(block).unwrap();
    let term = dictionary.terms().unwrap().next().unwrap().unwrap();
    meta.set_rewrite_journal(Some(RewriteJournal {
        head: term.head,
        tail: term.tail,
        phase: RewritePhase::Building,
    }))
    .unwrap();
    store.commit(&[&meta]).unwrap();
    let before = store.pages.clone();
    assert!(mutable::recover_compaction(&mut store).is_err());
    assert_eq!(store.pages, before);
    assert_eq!(scan(&mut store, "alpha").unwrap().len(), 700);
}

#[test]
fn cyclic_posting_chains_fail_instead_of_spinning() {
    let mut store = seed(1100, "alpha");
    let meta = store.read(0).unwrap();
    let (block, _) = meta.bucket(bucket_for("alpha")).unwrap();
    let dictionary = store.read(block).unwrap();
    let term = dictionary.terms().unwrap().next().unwrap().unwrap();
    let first = store.read(term.head).unwrap();
    let mut second = store.read(first.next().unwrap()).unwrap();
    assert_ne!(second.block(), term.tail);
    second.set_next(first.block()).unwrap();
    store.commit(&[&second]).unwrap();
    assert!(scan(&mut store, "alpha").is_err());
    assert!(mutable::compact(&mut store).is_err());
}
