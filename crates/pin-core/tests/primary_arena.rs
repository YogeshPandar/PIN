#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::mutable::PageStore;
use pin_core::primary::PrimaryArena;

#[test]
fn arena_packs_checked_extents_without_crossing_a_pg_page() {
    let mut store = MemoryStore::default();
    pin_core::mutable::initialize(&mut store).unwrap();
    let block = store.extend().unwrap();
    let mut arena = PrimaryArena::new(block).unwrap();
    let first = vec![0x11; 3_000];
    let second = vec![0x22; 3_000];
    let third = vec![0x33; 3_000];
    let a = arena.try_push(&first).unwrap().unwrap();
    let b = arena.try_push(&second).unwrap().unwrap();
    assert!(arena.try_push(&third).unwrap().is_none());
    let page = arena.finish().unwrap();
    store.commit(&[&page]).unwrap();
    let read = store.read(block).unwrap();
    assert_eq!(read.primary_extent(a.offset, a.len).unwrap(), first);
    assert_eq!(read.primary_extent(b.offset, b.len).unwrap(), second);
    assert_eq!(a.offset, 16);
    assert_eq!(b.offset, 3_016);
    assert_eq!(read.primary_payload().unwrap().len(), 6_000);

    let next = store.extend().unwrap();
    let mut arena = PrimaryArena::new(next).unwrap();
    let c = arena.try_push(&third).unwrap().unwrap();
    assert_eq!(c.offset, 16);
    let page = arena.finish().unwrap();
    store.commit(&[&page]).unwrap();
    assert_eq!(
        store
            .read(next)
            .unwrap()
            .primary_extent(c.offset, c.len)
            .unwrap(),
        third
    );
}

#[test]
fn arena_rejects_invalid_payload_without_consuming_space() {
    assert!(PrimaryArena::new(0).is_err());
    let mut arena = PrimaryArena::new(1).unwrap();
    assert!(arena.try_push(&[]).is_err());
    assert!(arena.try_push(&vec![0; 8_153]).is_err());
    let extent = arena.try_push(&[7]).unwrap().unwrap();
    assert_eq!(extent.offset, 16);
    assert_eq!(arena.finish().unwrap().primary_payload().unwrap(), &[7]);
}
