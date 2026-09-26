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

#[test]
fn inline_singletons_intersect_without_posting_page_reads() {
    let mut store = CountStore::default();
    mutable::initialize(&mut store).unwrap();
    let key = GroupKey::new(
        Generation::new(1).unwrap(),
        SegmentId::new(1).unwrap(),
        256,
        store.layout(),
    )
    .unwrap();
    let left = encode_directory(key, 1, &[PageDescriptor::singleton(7, 12)]).unwrap();
    let right = encode_directory(key, 2, &[PageDescriptor::singleton(7, 12)]).unwrap();
    let directories = [
        Directory::open(&left).unwrap(),
        Directory::open(&right).unwrap(),
    ];
    let mut output = Vec::new();
    let work = scan_and(&mut store, key, &directories, |block, mask| {
        output.push((block, mask));
        Ok(())
    })
    .unwrap();
    assert_eq!(work.containers_read, 0);
    assert_eq!(work.candidate_offsets, 1);
    assert_eq!(store.reads, 0);
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].0, 263);
    assert_eq!(output[0].1[0], 1 << 11);

    let other = encode_directory(key, 3, &[PageDescriptor::singleton(7, 13)]).unwrap();
    let directories = [
        Directory::open(&left).unwrap(),
        Directory::open(&other).unwrap(),
    ];
    let work = scan_and(&mut store, key, &directories, |_, _| {
        panic!("unexpected match")
    })
    .unwrap();
    assert_eq!(work.containers_read, 0);
    assert_eq!(work.candidate_offsets, 0);
    assert_eq!(store.reads, 0);
}

#[test]
fn boolean_scans_reuse_two_packed_posting_pages_across_many_heap_pages() {
    let mut store = CountStore::default();
    mutable::initialize(&mut store).unwrap();
    let key = GroupKey::new(
        Generation::new(3).unwrap(),
        SegmentId::new(2).unwrap(),
        256,
        store.layout(),
    )
    .unwrap();

    let mut left_payload = Vec::new();
    let mut right_payload = Vec::new();
    let mut left_entries = Vec::new();
    let mut right_entries = Vec::new();
    let mut expected_and = Vec::new();
    let mut expected_or = Vec::new();
    for page in 0..120u8 {
        let left_offset = u16::from(page) + 1;
        let right_offset = if page.is_multiple_of(2) {
            left_offset
        } else {
            left_offset + 1
        };
        let (left_kind, left_bytes) =
            encode_offsets(key.layout().max_offset(), &[left_offset]).unwrap();
        let (right_kind, right_bytes) =
            encode_offsets(key.layout().max_offset(), &[right_offset]).unwrap();
        assert_eq!(left_bytes.len(), 2);
        assert_eq!(right_bytes.len(), 2);
        left_entries.push(PageDescriptor {
            page,
            kind: left_kind,
            count: 1,
            extent: Extent {
                block: 0,
                offset: 16 + u16::from(page) * 2,
                len: 2,
            },
        });
        right_entries.push(PageDescriptor {
            page,
            kind: right_kind,
            count: 1,
            extent: Extent {
                block: 0,
                offset: 16 + u16::from(page) * 2,
                len: 2,
            },
        });
        left_payload.extend_from_slice(&left_bytes);
        right_payload.extend_from_slice(&right_bytes);

        let mut left_mask = [0u64; 8];
        let left_bit = usize::from(left_offset - 1);
        left_mask[left_bit / 64] |= 1 << (left_bit % 64);
        let mut right_mask = [0u64; 8];
        let right_bit = usize::from(right_offset - 1);
        right_mask[right_bit / 64] |= 1 << (right_bit % 64);
        expected_or.push((256 + u32::from(page), {
            let mut mask = left_mask;
            for (word, rhs) in mask.iter_mut().zip(right_mask) {
                *word |= rhs;
            }
            mask
        }));
        if page.is_multiple_of(2) {
            expected_and.push((256 + u32::from(page), left_mask));
        }
    }

    let left_block = store.extend().unwrap();
    for entry in &mut left_entries {
        entry.extent.block = left_block;
    }
    let left_page = Page::primary(left_block, &left_payload).unwrap();
    store.commit(&[&left_page]).unwrap();
    let right_block = store.extend().unwrap();
    for entry in &mut right_entries {
        entry.extent.block = right_block;
    }
    let right_page = Page::primary(right_block, &right_payload).unwrap();
    store.commit(&[&right_page]).unwrap();
    let left_bytes = encode_directory(key, 1, &left_entries).unwrap();
    let right_bytes = encode_directory(key, 2, &right_entries).unwrap();
    let left = Directory::open(&left_bytes).unwrap();
    let right = Directory::open(&right_bytes).unwrap();
    let directories = [left, right];

    let mut and_rows = Vec::new();
    let and_work = scan_and(&mut store, key, &directories, |block, mask| {
        and_rows.push((block, mask));
        Ok(())
    })
    .unwrap();
    assert_eq!(and_work.selected_pages, 120);
    assert_eq!(and_work.containers_read, 240);
    assert_eq!(and_work.emitted_pages as usize, expected_and.len());
    assert_eq!(and_rows, expected_and);
    assert_eq!(store.reads, 2, "AND reads each shared packed block once");

    let mut or_rows = Vec::new();
    let or_work = scan_or(&mut store, key, &directories, |block, mask| {
        or_rows.push((block, mask));
        Ok(())
    })
    .unwrap();
    assert_eq!(or_work.selected_pages, 120);
    assert_eq!(or_work.containers_read, 240);
    assert_eq!(or_work.emitted_pages as usize, expected_or.len());
    assert_eq!(or_rows, expected_or);
    assert_eq!(store.reads, 4, "OR reads each shared packed block once");
}
