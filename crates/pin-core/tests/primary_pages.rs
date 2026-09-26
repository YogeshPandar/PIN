#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::error::Result;
use pin_core::grouped::GroupKey;
use pin_core::identity::{Generation, HeapLayout, SegmentId};
use pin_core::mutable::page::Page;
use pin_core::mutable::{self, PageStore};
use pin_core::primary::{
    Directory, Extent, PageDescriptor, encode_directory, encode_offsets, read_offsets,
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

#[test]
fn selected_container_loads_one_physical_payload_page() {
    let mut store = CountStore::default();
    mutable::initialize(&mut store).unwrap();
    let key = GroupKey::new(
        Generation::new(1).unwrap(),
        SegmentId::new(1).unwrap(),
        0,
        store.layout(),
    )
    .unwrap();
    let (kind_a, bytes_a) = encode_offsets(291, &[1, 7]).unwrap();
    let dense: Vec<u16> = (1..=100).collect();
    let (kind_b, bytes_b) = encode_offsets(291, &dense).unwrap();
    let block_a = store.extend().unwrap();
    let page_a = Page::primary(block_a, &bytes_a).unwrap();
    store.commit(&[&page_a]).unwrap();
    let block_b = store.extend().unwrap();
    let page_b = Page::primary(block_b, &bytes_b).unwrap();
    store.commit(&[&page_b]).unwrap();
    let descriptors = [
        PageDescriptor {
            page: 4,
            kind: kind_a,
            count: 2,
            extent: Extent {
                block: block_a,
                offset: 16,
                len: bytes_a.len() as u16,
            },
        },
        PageDescriptor {
            page: 200,
            kind: kind_b,
            count: 100,
            extent: Extent {
                block: block_b,
                offset: 16,
                len: bytes_b.len() as u16,
            },
        },
    ];
    let encoded = encode_directory(key, 7, &descriptors).unwrap();
    let directory_block = store.extend().unwrap();
    let directory_page = Page::primary(directory_block, &encoded).unwrap();
    store.commit(&[&directory_page]).unwrap();

    let image = store.read(directory_block).unwrap();
    image.validate(store.layout()).unwrap();
    let directory = Directory::open(image.primary_payload().unwrap()).unwrap();
    assert!(
        read_offsets(&mut store, key, directory, 5)
            .unwrap()
            .is_none()
    );
    assert_eq!(store.reads, 1);
    let wrong_key = GroupKey::new(
        Generation::new(2).unwrap(),
        SegmentId::new(1).unwrap(),
        0,
        store.layout(),
    )
    .unwrap();
    assert!(read_offsets(&mut store, wrong_key, directory, 4).is_err());
    assert_eq!(store.reads, 1);
    let offsets = read_offsets(&mut store, key, directory, 4)
        .unwrap()
        .unwrap();
    assert_eq!(offsets[0], (1 << 0) | (1 << 6));
    assert_eq!(store.reads, 2);
    assert_eq!(
        read_offsets(&mut store, key, directory, 200)
            .unwrap()
            .unwrap()[0]
            .count_ones(),
        64
    );
    assert_eq!(store.reads, 3);

    let mut invalid_extent = descriptors;
    invalid_extent[0].extent.offset = 500;
    let bytes = encode_directory(key, 7, &invalid_extent).unwrap();
    let invalid_directory = Directory::open(&bytes).unwrap();
    assert!(read_offsets(&mut store, key, invalid_directory, 4).is_err());

    store.inner.pages[block_a as usize][6] = 6;
    assert!(read_offsets(&mut store, key, directory, 4).is_err());
}
