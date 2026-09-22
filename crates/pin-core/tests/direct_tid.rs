#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::Result;
use pin_core::identity::{HeapLayout, Incarnation, RootTid};
use pin_core::mutable::document::PreparedDocument;
use pin_core::mutable::page::{OwnerRef, Page, PageKind, SealedBuilder, TermRef};
use pin_core::mutable::{self, CompactMode, PageStore};
use pin_core::query::{Query, QueryLimits};
use std::collections::BTreeSet;

fn root(i: u32) -> RootTid {
    RootTid::new(i / 200, (i % 200 + 1) as u16, HeapLayout::new(291).unwrap()).unwrap()
}

fn prepared(text: &str) -> PreparedDocument {
    PreparedDocument::prepare(
        &Analyzed::analyze(text, AnalysisLimits::default()).unwrap(),
        1 << 20,
    )
    .unwrap()
}

fn seed(count: u32) -> MemoryStore {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let docs = [
        prepared("alpha beta"),
        prepared("alpha gamma"),
        prepared("beta"),
    ];
    for i in 0..count {
        mutable::insert(&mut store, root(i), &docs[i as usize % 3]).unwrap();
    }
    store.events.clear();
    store
}

fn scan(store: &mut impl PageStore, source: &str) -> BTreeSet<RootTid> {
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let mut found = BTreeSet::new();
    mutable::scan_query_with_recheck(store, &query, 1 << 20, |root, recheck| {
        assert!(!recheck);
        found.insert(root);
        Ok(())
    })
    .unwrap();
    found
}

fn verify(store: &mut MemoryStore, documents: &[(RootTid, &str)]) {
    let mut reference = MemoryStore::default();
    mutable::initialize(&mut reference).unwrap();
    for &(root, text) in documents {
        mutable::insert(&mut reference, root, &prepared(text)).unwrap();
    }
    for query in [
        "alpha",
        "beta",
        "gamma",
        "replacement",
        "missing",
        "alpha AND beta",
        "alpha OR gamma",
        "(alpha OR beta) AND gamma",
        "alpha AND alpha",
        "alpha OR alpha",
    ] {
        assert_eq!(scan(store, query), scan(&mut reference, query), "{query}");
    }
}

#[test]
fn direct_codec_roundtrip_tombstones_and_truncation() {
    let layout = HeapLayout::new(291).unwrap();
    let mut builder = SealedBuilder::new_direct(
        22,
        TermRef {
            page: 2,
            offset: 16,
        },
    )
    .unwrap();
    let mut expected = Vec::new();
    for i in 0..2000 {
        let owner = OwnerRef {
            page: 1 + i / 100,
            slot: (i % 100) as u16,
            incarnation: Incarnation::new(u64::from(i) + 1).unwrap(),
        };
        if !builder.push_direct(owner, root(i)).unwrap() {
            break;
        }
        expected.push(owner);
    }
    assert!(expected.len() > 500);
    let mut page = builder.finish().unwrap();
    page.validate(layout).unwrap();
    assert_eq!(
        page.posting_refs()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap(),
        expected
    );
    for i in 0..expected.len() {
        assert_eq!(
            page.direct_root(i as u16, layout).unwrap(),
            Some(root(i as u32))
        );
        if i % 3 == 0 {
            page.remove_direct_root(i as u16, layout).unwrap();
        }
    }
    page.validate(layout).unwrap();
    assert_eq!(
        page.posting_refs()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap(),
        expected
    );
    for len in 1..page.bytes().len() {
        let result = Page::read_with(22, |out| {
            out[..len].copy_from_slice(&page.bytes()[..len]);
            Ok(len)
        });
        assert!(
            result.and_then(|p| p.validate(layout)).is_err(),
            "truncation {len}"
        );
    }
    for byte in [56 + 6, 56 + 7] {
        let mut damaged = page.bytes().to_vec();
        damaged[byte] = 2;
        let decoded = Page::read_with(22, |out| {
            out[..damaged.len()].copy_from_slice(&damaged);
            Ok(damaged.len())
        })
        .unwrap();
        assert!(decoded.validate(layout).is_err());
    }
}

#[test]
fn boolean_scans_mix_direct_and_mutable_then_reuse_heap_coordinates() {
    let mut store = seed(1800);
    let before: Vec<_> = (0..1800)
        .map(|i| {
            (
                root(i),
                ["alpha beta", "alpha gamma", "beta"][i as usize % 3],
            )
        })
        .collect();
    mutable::compact_with_mode(&mut store, CompactMode::DirectTid).unwrap();
    assert!(store.pages.iter().any(|p| p.get(6) == Some(&9)));
    verify(&mut store, &before);
    for i in 1800..1900 {
        mutable::insert(&mut store, root(i), &prepared("alpha gamma")).unwrap();
    }
    mutable::vacuum(&mut store, |tid| Ok(tid.offset() % 2 == 1)).unwrap();
    let mut after = Vec::new();
    for i in 0..1900 {
        let text = if root(i).offset() % 2 == 1 {
            mutable::insert(&mut store, root(i), &prepared("replacement")).unwrap();
            "replacement"
        } else if i < 1800 {
            ["alpha beta", "alpha gamma", "beta"][i as usize % 3]
        } else {
            "alpha gamma"
        };
        after.push((root(i), text));
    }
    verify(&mut store, &after);
    mutable::compact_with_mode(&mut store, CompactMode::DirectTid).unwrap();
    verify(&mut store, &after);
    mutable::vacuum(&mut store, |_| Ok(false)).unwrap();
    verify(&mut store, &after);
}

#[test]
fn every_interrupted_bulk_delete_resumes_before_heap_reuse() {
    let mut original = seed(240);
    mutable::compact_with_mode(&mut original, CompactMode::DirectTid).unwrap();
    original.events.clear();
    let mut complete = original.clone();
    mutable::vacuum(&mut complete, |_| Ok(true)).unwrap();
    assert!(complete.events.contains(&mutable::Stage::DirectRemoved));
    for fail_at in 0..complete.events.len() {
        let mut store = original.clone();
        store.fail_at = Some(fail_at);
        assert!(mutable::vacuum(&mut store, |_| Ok(true)).is_err());
        store.fail_at = None;
        // a failed bulk-delete pass never authorizes the host to reuse slots.
        mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
        for i in 0..240 {
            mutable::insert(&mut store, root(i), &prepared("replacement")).unwrap();
        }
        assert!(
            scan(&mut store, "alpha OR beta OR gamma").is_empty(),
            "stage {fail_at}"
        );
        assert_eq!(scan(&mut store, "replacement").len(), 240);
    }
}

#[test]
fn every_direct_compaction_transition_recovers_without_losing_documents() {
    let original = seed(240);
    let mut complete = original.clone();
    mutable::compact_with_mode(&mut complete, CompactMode::DirectTid).unwrap();
    let expected = scan(&mut complete, "alpha OR beta");
    for fail_at in 0..complete.events.len() {
        let mut store = original.clone();
        store.fail_at = Some(fail_at);
        assert!(mutable::compact_with_mode(&mut store, CompactMode::DirectTid).is_err());
        store.fail_at = None;
        mutable::recover_compaction(&mut store).unwrap();
        mutable::vacuum(&mut store, |_| Ok(false)).unwrap();
        mutable::compact_with_mode(&mut store, CompactMode::DirectTid).unwrap();
        assert_eq!(
            scan(&mut store, "alpha OR beta"),
            expected,
            "stage {fail_at}"
        );
    }
}

struct CountReads {
    store: MemoryStore,
    owners: usize,
}
impl PageStore for CountReads {
    fn layout(&self) -> HeapLayout {
        self.store.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.store.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        let page = self.store.read(block)?;
        self.owners += usize::from(page.kind() == PageKind::Owners);
        Ok(page)
    }
    fn extend(&mut self) -> Result<u32> {
        self.store.extend()
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        self.store.commit(pages)
    }
}

#[test]
fn sealed_term_scans_eliminate_per_document_owner_page_resolution() {
    let mut baseline = seed(1800);
    mutable::compact(&mut baseline).unwrap();
    let mut direct = baseline.clone();
    mutable::compact_with_mode(&mut direct, CompactMode::DirectTid).unwrap();
    let mut a = CountReads {
        store: baseline,
        owners: 0,
    };
    let mut b = CountReads {
        store: direct,
        owners: 0,
    };
    assert_eq!(scan(&mut a, "alpha"), scan(&mut b, "alpha"));
    assert!(a.owners > 10);
    assert_eq!(
        b.owners, 1,
        "only the dictionary's first owner needs resolution"
    );
}

#[test]
fn selective_and_seeks_across_direct_page_ranges() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let common = prepared("alpha");
    let rare = prepared("alpha rareplanet");
    for i in 0..1200 {
        mutable::insert(&mut store, root(i), if i == 1199 { &rare } else { &common }).unwrap();
    }
    mutable::compact_with_mode(&mut store, CompactMode::DirectTid).unwrap();
    assert_eq!(
        scan(&mut store, "alpha AND rareplanet"),
        BTreeSet::from([root(1199)])
    );
    assert!(scan(&mut store, "alpha AND missing").is_empty());
}
