#[path = "support/mutable_store.rs"]
mod mutable_store;
use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::Result;
use pin_core::identity::RootTid;
use pin_core::mutable::document::PreparedDocument;
use pin_core::mutable::page::{NO_BLOCK, Page, PageKind};
use pin_core::mutable::{self, PageStore, Stage};
use pin_core::query::{Query, QueryLimits};

fn prepared(text: &str) -> PreparedDocument {
    PreparedDocument::prepare(
        &Analyzed::analyze(text, AnalysisLimits::default()).unwrap(),
        64 << 20,
    )
    .unwrap()
}
fn root(store: &MemoryStore, n: u16) -> RootTid {
    RootTid::new(1, n, store.layout()).unwrap()
}
fn scan<S: PageStore>(store: &mut S, query: &str, budget: usize) -> Vec<(RootTid, bool)> {
    let query = Query::parse(query, QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    mutable::scan_query_with_options(store, &query, budget, true, |tid, recheck| {
        rows.push((tid, recheck));
        Ok(())
    })
    .unwrap();
    rows
}
struct Count {
    inner: MemoryStore,
    payload_reads: usize,
}
impl PageStore for Count {
    fn layout(&self) -> pin_core::identity::HeapLayout {
        self.inner.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, b: u32) -> Result<Page> {
        let page = self.inner.read(b)?;
        self.payload_reads += usize::from(matches!(
            page.kind(),
            PageKind::Fragment | PageKind::DocumentDirectory
        ));
        Ok(page)
    }
    fn read_into(&mut self, b: u32, page: &mut Page) -> Result<()> {
        self.inner.read_into(b, page)?;
        self.payload_reads += usize::from(matches!(
            page.kind(),
            PageKind::Fragment | PageKind::DocumentDirectory
        ));
        Ok(())
    }
    fn extend(&mut self) -> Result<u32> {
        self.inner.extend()
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        self.inner.commit(pages)
    }
}

#[test]
fn rare_late_phrase_skips_unrelated_physical_position_extents() {
    let mut inner = MemoryStore {
        direct_documents: true,
        ..Default::default()
    };
    mutable::initialize(&mut inner).unwrap();
    inner.direct_documents = false; // persisted format controls later insertions.
    let tid = root(&inner, 1);
    let text = format!("alpha {}zulu omega", "middle ".repeat(60_000));
    let document = prepared(&text);
    let owner = mutable::insert(&mut inner, tid, &document).unwrap();
    assert!(inner.read(0).unwrap().direct_documents().unwrap());
    let head = inner
        .read(owner.page)
        .unwrap()
        .owner(owner.slot, inner.layout())
        .unwrap()
        .data_head;
    let page = inner.read(head).unwrap();
    let directory = page.document_directory_data().unwrap();
    assert!(directory.fragments() > 4);
    let mut store = Count {
        inner,
        payload_reads: 0,
    };
    assert_eq!(
        scan(&mut store, "\"zulu omega\"", 1 << 20),
        vec![(tid, false)]
    );
    assert_eq!(
        store.payload_reads, 2,
        "head plus last extent, without intervening tails"
    );
    assert!(scan(&mut store, "\"alpha omega\"", 1 << 20).is_empty());
    assert_eq!(
        scan(&mut store, "\"middle middle\"", 1 << 20),
        vec![(tid, false)]
    );
    assert_eq!(
        scan(&mut store, "\"zulu omega\"", 16 << 10),
        vec![(tid, true)]
    );
}

#[test]
fn direct_and_legacy_readers_match_an_independent_text_oracle() {
    for direct in [false, true] {
        let mut store = MemoryStore {
            direct_documents: direct,
            ..Default::default()
        };
        mutable::initialize(&mut store).unwrap();
        let texts: Vec<_> = (0..12)
            .map(|n| {
                format!(
                    "{} {} {}",
                    "middle ".repeat(10_000 + n),
                    if n % 2 == 0 {
                        "zulu omega"
                    } else {
                        "omega zulu"
                    },
                    "echo ".repeat(n * 7)
                )
            })
            .collect();
        for (n, text) in texts.iter().enumerate() {
            let tid = root(&store, n as u16 + 1);
            mutable::insert(&mut store, tid, &prepared(text)).unwrap();
        }
        for source in [
            "\"zulu omega\"",
            "\"omega zulu\"",
            "\"middle zulu\"",
            "\"echo echo\"",
            "\"omega echo\"",
        ] {
            let words: Vec<_> = source.trim_matches('"').split_whitespace().collect();
            let expected: Vec<_> = texts
                .iter()
                .enumerate()
                .filter_map(|(n, text)| {
                    let tokens: Vec<_> = text.split_whitespace().collect();
                    tokens
                        .windows(words.len())
                        .any(|w| w == words)
                        .then(|| (root(&store, n as u16 + 1), false))
                })
                .collect();
            assert_eq!(scan(&mut store, source, 1 << 20), expected);
        }
    }
}

#[test]
fn every_directory_publication_boundary_recovers_and_reuses_free_pages() {
    let mut baseline = MemoryStore {
        direct_documents: true,
        ..Default::default()
    };
    mutable::initialize(&mut baseline).unwrap();
    let keep = root(&baseline, 1);
    let attempted = root(&baseline, 2);
    mutable::insert(&mut baseline, keep, &prepared("stable")).unwrap();
    let document = prepared(&format!("{}zulu omega", "middle ".repeat(24_000)));
    let mut complete = baseline.clone();
    complete.events.clear();
    mutable::insert(&mut complete, attempted, &document).unwrap();
    for step in 0..complete.events.len() {
        let mut store = baseline.clone();
        store.events.clear();
        store.fail_at = Some(step);
        assert!(mutable::insert(&mut store, attempted, &document).is_err());
        store.fail_at = None;
        let published = store.events.last() == Some(&Stage::Published);
        assert_eq!(
            !scan(&mut store, "\"zulu omega\"", 1 << 20).is_empty(),
            published
        );
        mutable::vacuum(&mut store, |tid| Ok(tid == attempted)).unwrap();
        assert!(scan(&mut store, "\"zulu omega\"", 1 << 20).is_empty());
        mutable::insert(&mut store, attempted, &document).unwrap();
        assert_eq!(
            scan(&mut store, "\"zulu omega\"", 1 << 20),
            vec![(attempted, false)]
        );
        assert_eq!(scan(&mut store, "stable", 1 << 20), vec![(keep, false)]);
    }
}

#[test]
fn consumed_extent_identity_and_mapping_corruption_fail_closed() {
    let mut store = MemoryStore {
        direct_documents: true,
        ..Default::default()
    };
    mutable::initialize(&mut store).unwrap();
    let tid = root(&store, 1);
    let owner = mutable::insert(
        &mut store,
        tid,
        &prepared(&format!("{}zulu omega", "middle ".repeat(60_000))),
    )
    .unwrap();
    let head = store
        .read(owner.page)
        .unwrap()
        .owner(owner.slot, store.layout())
        .unwrap()
        .data_head;
    let page = store.read(head).unwrap();
    let directory = page.document_directory_data().unwrap();
    let last = directory.block(directory.fragments() - 1).unwrap();
    let saved = store.pages[last as usize].clone();
    store.pages[last as usize][32..36].copy_from_slice(&0u32.to_le_bytes());
    let query = Query::parse("\"zulu omega\"", QueryLimits::default()).unwrap();
    assert!(
        mutable::scan_query_with_options(&mut store, &query, 1 << 20, true, |_, _| Ok(())).is_err()
    );
    store.pages[last as usize] = saved;
    store.pages[head as usize][48..52].copy_from_slice(&NO_BLOCK.to_le_bytes());
    assert!(store.read(head).unwrap().validate(store.layout()).is_err());
    store.pages[0][28..32].copy_from_slice(&2u32.to_le_bytes());
    assert!(store.read(0).unwrap().validate(store.layout()).is_err());
}

#[test]
fn every_directory_vacuum_boundary_is_idempotent() {
    let mut baseline = MemoryStore {
        direct_documents: true,
        ..Default::default()
    };
    mutable::initialize(&mut baseline).unwrap();
    let tid = root(&baseline, 1);
    let document = prepared(&format!("{}zulu omega", "middle ".repeat(24_000)));
    mutable::insert(&mut baseline, tid, &document).unwrap();
    let mut complete = baseline.clone();
    complete.events.clear();
    mutable::vacuum(&mut complete, |_| Ok(true)).unwrap();
    for step in 0..complete.events.len() {
        let mut store = baseline.clone();
        store.events.clear();
        store.fail_at = Some(step);
        assert!(mutable::vacuum(&mut store, |_| Ok(true)).is_err());
        store.fail_at = None;
        mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
        let again = mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
        assert_eq!(again.reclaimed_pages, 0);
        assert!(scan(&mut store, "\"zulu omega\"", 1 << 20).is_empty());
        let next = root(&store, 2);
        mutable::insert(&mut store, next, &document).unwrap();
        assert_eq!(
            scan(&mut store, "\"zulu omega\"", 1 << 20),
            vec![(next, false)]
        );
    }
}

#[test]
fn directory_codec_covers_maximum_extent_count_and_truncation() {
    use pin_core::identity::Incarnation;
    use pin_core::mutable::document::MAX_DOCUMENT_BYTES;
    use pin_core::mutable::page::{MAX_DIRECT_FRAGMENTS, OwnerRef, direct_document_layout};
    let owner = OwnerRef {
        page: 1,
        slot: 0,
        incarnation: Incarnation::new(1).unwrap(),
    };
    let blocks: Vec<u32> = (3..3 + MAX_DIRECT_FRAGMENTS as u32).collect();
    let page = Page::document_directory(
        2,
        owner,
        pin_core::mutable::document::MAX_DOCUMENT_BYTES,
        &blocks,
        &vec![0; direct_document_layout(MAX_DOCUMENT_BYTES).unwrap().0],
    )
    .unwrap();
    let directory = page.document_directory_data().unwrap();
    assert_eq!(directory.fragments(), MAX_DIRECT_FRAGMENTS);
    assert!(directory.block(MAX_DIRECT_FRAGMENTS).is_err());
    for length in 0..page.bytes().len() {
        let candidate = Page::read_with(2, |out| {
            out[..length].copy_from_slice(&page.bytes()[..length]);
            Ok(length)
        });
        if length != 0 {
            assert!(
                candidate
                    .and_then(|p| p.document_directory_data().map(|_| ()))
                    .is_err()
            );
        }
    }
    let mut wrong = blocks.clone();
    wrong[0] = 2;
    assert!(
        Page::document_directory(
            2,
            owner,
            pin_core::mutable::document::MAX_DOCUMENT_BYTES,
            &wrong,
            &vec![0; direct_document_layout(MAX_DOCUMENT_BYTES).unwrap().0]
        )
        .is_err()
    );
}

#[test]
fn selected_headers_cross_prefix_and_tail_boundaries_without_changing_phrases() {
    for count in (8064..8090).chain(16190..16222) {
        let mut store = MemoryStore {
            direct_documents: true,
            ..Default::default()
        };
        mutable::initialize(&mut store).unwrap();
        let tid = root(&store, 1);
        let text = format!(
            "{}omega {}",
            "middle ".repeat(count),
            "zulu ".repeat(20_000)
        );
        mutable::insert(&mut store, tid, &prepared(&text)).unwrap();
        assert_eq!(
            scan(&mut store, "\"omega zulu\"", 1 << 20),
            vec![(tid, false)]
        );
        assert!(scan(&mut store, "\"zulu omega\"", 1 << 20).is_empty());
    }
    let mut store = MemoryStore {
        direct_documents: true,
        ..Default::default()
    };
    mutable::initialize(&mut store).unwrap();
    let tid = root(&store, 1);
    mutable::insert(
        &mut store,
        tid,
        &prepared(&format!("{}éclair café", "middle ".repeat(12000))),
    )
    .unwrap();
    assert_eq!(
        scan(&mut store, "\"éclair café\"", 1 << 20),
        vec![(tid, false)]
    );
}

#[test]
fn selected_extents_fit_a_budget_smaller_than_the_complete_document() {
    let document = prepared(&format!("{}zulu omega", "middle ".repeat(80_000)));
    assert!(document.bytes().len() > 64 << 10);
    for direct in [false, true] {
        let mut store = MemoryStore {
            direct_documents: direct,
            ..Default::default()
        };
        mutable::initialize(&mut store).unwrap();
        let tid = root(&store, 1);
        mutable::insert(&mut store, tid, &document).unwrap();
        assert_eq!(
            scan(&mut store, "\"zulu omega\"", 64 << 10),
            vec![(tid, !direct)]
        );
    }
}

#[test]
fn adaptive_directory_layout_covers_all_document_lengths() {
    use pin_core::mutable::document::MAX_DOCUMENT_BYTES;
    use pin_core::mutable::page::{
        CAPACITY, FRAGMENT_BYTES, INLINE_BYTES, MAX_DIRECT_FRAGMENTS, direct_document_layout,
    };
    for total in INLINE_BYTES + 1..=MAX_DOCUMENT_BYTES {
        let (prefix, count) = direct_document_layout(total).unwrap();
        assert!(prefix <= total);
        assert!(48 + count * 4 + prefix <= CAPACITY);
        assert_eq!(count, (total - prefix).div_ceil(FRAGMENT_BYTES));
        assert!(count <= MAX_DIRECT_FRAGMENTS);
        if count > 0 {
            assert_eq!(48 + count * 4 + prefix, CAPACITY);
        }
    }
}

#[test]
fn directory_can_hold_a_document_without_tail_pages() {
    use pin_core::identity::Incarnation;
    use pin_core::mutable::page::{INLINE_BYTES, NO_BLOCK, OwnerRef, direct_document_layout};
    let owner = OwnerRef {
        page: 1,
        slot: 0,
        incarnation: Incarnation::new(1).unwrap(),
    };
    let total = INLINE_BYTES + 1;
    let (prefix, count) = direct_document_layout(total).unwrap();
    assert_eq!((prefix, count), (total, 0));
    let page = Page::document_directory(2, owner, total, &[], &vec![0; prefix]).unwrap();
    assert_eq!(page.next().unwrap(), NO_BLOCK);
    assert_eq!(page.document_directory_data().unwrap().fragments(), 0);
}

#[test]
fn fixed_prefix_version_remains_readable_and_unknown_versions_fail() {
    use pin_core::identity::Incarnation;
    use pin_core::mutable::page::{DIRECT_PREFIX_BYTES, OwnerRef, direct_document_layout};
    let owner = OwnerRef {
        page: 1,
        slot: 0,
        incarnation: Incarnation::new(1).unwrap(),
    };
    let total = 10_000;
    let prefix = direct_document_layout(total).unwrap().0;
    let current = Page::document_directory(2, owner, total, &[3], &vec![0; prefix]).unwrap();
    let mut old = current.bytes()[..52 + DIRECT_PREFIX_BYTES].to_vec();
    old[40..44].copy_from_slice(&(DIRECT_PREFIX_BYTES as u32).to_le_bytes());
    old[44..48].copy_from_slice(&0u32.to_le_bytes());
    let page = Page::read_with(2, |out| {
        out[..old.len()].copy_from_slice(&old);
        Ok(old.len())
    })
    .unwrap();
    let directory = page.document_directory_data().unwrap();
    assert_eq!(directory.prefix.len(), DIRECT_PREFIX_BYTES);
    assert_eq!(directory.block(0).unwrap(), 3);
    old[44..48].copy_from_slice(&2u32.to_le_bytes());
    let page = Page::read_with(2, |out| {
        out[..old.len()].copy_from_slice(&old);
        Ok(old.len())
    })
    .unwrap();
    assert!(page.document_directory_data().is_err());
}
