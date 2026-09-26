#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::error::Result;
use pin_core::grouped::GroupKey;
use pin_core::identity::{Generation, HeapLayout, SegmentId};
use pin_core::mutable::page::Page;
use pin_core::mutable::{self, PageStore};
use pin_core::primary::{
    Directory, Extent, PageDescriptor, encode_directory, encode_offsets, scan_and, scan_or,
};

#[derive(Default)]
struct CountStore {
    inner: MemoryStore,
    reads: usize,
}

impl PageStore for CountStore {
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }

    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }

    fn read(&mut self, block: u32) -> Result<Page> {
        self.reads += 1;
        self.inner.read(block)
    }

    fn extend(&mut self) -> Result<u32> {
        self.inner.extend()
    }

    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        self.inner.commit(pages)
    }
}

fn term(store: &mut CountStore, key: GroupKey, number: u64, pages: &[(u8, &[u16])]) -> Vec<u8> {
    let mut descriptors = Vec::new();
    for &(page, offsets) in pages {
        let (kind, bytes) = encode_offsets(key.layout().max_offset(), offsets).unwrap();
        let block = store.extend().unwrap();
        let image = Page::primary(block, &bytes).unwrap();
        store.commit(&[&image]).unwrap();
        descriptors.push(PageDescriptor {
            page,
            kind,
            count: offsets.len() as u16,
            extent: Extent {
                block,
                offset: 16,
                len: bytes.len() as u16,
            },
        });
    }
    encode_directory(key, number, &descriptors).unwrap()
}

#[test]
fn and_prunes_pages_then_short_circuits_empty_offsets() {
    let mut store = CountStore::default();
    mutable::initialize(&mut store).unwrap();
    let key = GroupKey::new(
        Generation::new(1).unwrap(),
        SegmentId::new(1).unwrap(),
        256,
        store.layout(),
    )
    .unwrap();
    let a = term(
        &mut store,
        key,
        1,
        &[(1, &[1, 2, 3]), (4, &[1, 2]), (8, &[1])],
    );
    let b = term(&mut store, key, 2, &[(1, &[2, 3]), (4, &[3]), (9, &[1])]);
    let c = term(&mut store, key, 3, &[(1, &[3]), (4, &[3]), (10, &[1])]);
    let directories = [
        Directory::open(&a).unwrap(),
        Directory::open(&b).unwrap(),
        Directory::open(&c).unwrap(),
    ];
    let mut results = Vec::new();
    let work = scan_and(&mut store, key, &directories, |block, offsets| {
        results.push((block, offsets));
        Ok(())
    })
    .unwrap();
    assert_eq!(work.selected_pages, 2);
    assert_eq!(work.containers_read, 5);
    assert_eq!(work.emitted_pages, 1);
    assert_eq!(work.candidate_offsets, 1);
    assert_eq!(store.reads, 5);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, 257);
    assert_eq!(results[0].1[0], 1 << 2);

    let wrong = GroupKey::new(
        Generation::new(2).unwrap(),
        SegmentId::new(1).unwrap(),
        256,
        store.layout(),
    )
    .unwrap();
    assert!(scan_and(&mut store, wrong, &directories, |_, _| Ok(())).is_err());
    assert_eq!(store.reads, 5);

    let mut union = Vec::new();
    let work = scan_or(&mut store, key, &directories, |block, offsets| {
        union.push((block, offsets));
        Ok(())
    })
    .unwrap();
    assert_eq!(work.selected_pages, 5);
    assert_eq!(work.containers_read, 9);
    assert_eq!(work.emitted_pages, 5);
    assert_eq!(work.candidate_offsets, 9);
    assert_eq!(store.reads, 14);
    assert_eq!(
        union.iter().map(|(block, _)| *block).collect::<Vec<_>>(),
        vec![257, 260, 264, 265, 266]
    );
    assert_eq!(union[0].1[0], 0b111);
    assert_eq!(union[1].1[0], 0b111);
}
