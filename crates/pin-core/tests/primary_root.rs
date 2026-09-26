#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::identity::{Generation, HeapLayout};
use pin_core::mutable::PageStore;
use pin_core::mutable::page::{NO_BLOCK, Page, PageKind};
use pin_core::primary::{PrimaryRoot, initialize, read_root};

#[test]
fn v2_root_is_distinct_from_v1_and_persists_its_identity() {
    let mut store = MemoryStore::default();
    let generation = Generation::new(41).unwrap();
    let root = initialize(&mut store, generation).unwrap();
    assert_eq!(root, PrimaryRoot::empty(generation, store.layout()));
    assert_eq!(root.manifest_block, NO_BLOCK);
    assert_eq!(store.blocks().unwrap(), 1);
    assert_eq!(read_root(&mut store, generation).unwrap(), root);
    let page = store.read(0).unwrap();
    assert_eq!(page.kind(), PageKind::PrimaryMeta);
    assert!(read_root(&mut store, Generation::new(42).unwrap()).is_err());
    assert!(initialize(&mut store, generation).is_err());
}

#[test]
fn root_rejects_invalid_publication_and_corrupt_bytes() {
    let generation = Generation::new(1).unwrap();
    let layout = HeapLayout::new(291).unwrap();
    let empty = PrimaryRoot::empty(generation, layout);
    let mut bytes = empty.encode().unwrap();
    assert_eq!(PrimaryRoot::open(&bytes).unwrap(), empty);
    bytes[4] = 2;
    assert!(PrimaryRoot::open(&bytes).is_err());
    let invalid = PrimaryRoot {
        manifest_block: 2,
        segment_count: 0,
        ..empty
    };
    assert!(invalid.encode().is_err());
    let published = PrimaryRoot {
        epoch: 2,
        manifest_block: 2,
        segment_count: 1,
        ..empty
    };
    let page = Page::primary_metadata(published).unwrap();
    page.validate(layout).unwrap();
    assert_eq!(page.primary_root().unwrap(), published);
    assert!(page.validate(HeapLayout::new(512).unwrap()).is_err());
}
