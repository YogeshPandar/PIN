use pin_core::grouped::GroupKey;
use pin_core::identity::{Generation, HeapLayout, SegmentId};
use pin_core::primary::{
    ContainerKind, Directory, Extent, PageDescriptor, encode_directory, encode_offsets,
};
use std::collections::BTreeMap;

fn key(max_offset: u16) -> GroupKey {
    GroupKey::new(
        Generation::new(3).unwrap(),
        SegmentId::new(7).unwrap(),
        256,
        HeapLayout::new(max_offset).unwrap(),
    )
    .unwrap()
}

#[test]
fn selected_page_reads_only_its_extent_and_matches_independent_offsets() {
    let mut payloads = BTreeMap::new();
    let mut descriptors = Vec::new();
    let mut expected = BTreeMap::new();
    for page in 0..=255u8 {
        if page % 3 != 0 {
            continue;
        }
        let offsets: Vec<u16> = if page % 9 == 0 {
            (1..=291).filter(|offset| offset % 2 == 0).collect()
        } else {
            vec![1, u16::from(page) % 290 + 2]
        };
        let (kind, bytes) = encode_offsets(291, &offsets).unwrap();
        let block = u32::from(page) + 10;
        descriptors.push(PageDescriptor {
            page,
            kind,
            count: offsets.len() as u16,
            extent: Extent {
                block,
                offset: 0,
                len: bytes.len() as u16,
            },
        });
        payloads.insert(block, bytes);
        expected.insert(page, offsets);
    }
    let encoded = encode_directory(key(291), 42, &descriptors).unwrap();
    assert!(encoded.len() < 8192);
    let directory = Directory::open(&encoded).unwrap();
    assert_eq!(directory.key(), key(291));
    assert_eq!(directory.term(), 42);
    let mut fetches = 0;
    for page in 0..=255u8 {
        let mask = directory
            .offsets(page, |extent, output| {
                fetches += 1;
                output.copy_from_slice(&payloads[&extent.block]);
                Ok(())
            })
            .unwrap();
        if let Some(offsets) = expected.get(&page) {
            let mask = mask.unwrap();
            let actual: Vec<u16> = (1..=291)
                .filter(|offset| {
                    let bit = usize::from(offset - 1);
                    mask[bit / 64] & (1 << (bit % 64)) != 0
                })
                .collect();
            assert_eq!(&actual, offsets);
        } else {
            assert!(mask.is_none());
        }
    }
    assert_eq!(fetches, expected.len());
    let mut selected_fetches = 0;
    let _ = directory
        .offsets(252, |extent, output| {
            selected_fetches += 1;
            output.copy_from_slice(&payloads[&extent.block]);
            Ok(())
        })
        .unwrap();
    assert_eq!(selected_fetches, 1);
    assert_eq!(directory.page(253).unwrap(), None);
}

#[test]
fn corrupt_directory_and_selected_payload_fail_closed() {
    let (kind, payload) = encode_offsets(70, &[1, 3, 70]).unwrap();
    assert_eq!(kind, ContainerKind::Sparse);
    let descriptor = PageDescriptor {
        page: 5,
        kind,
        count: 3,
        extent: Extent {
            block: 22,
            offset: 17,
            len: payload.len() as u16,
        },
    };
    let encoded = encode_directory(key(70), 9, &[descriptor]).unwrap();
    for index in [0, 4, 8, 72, 76, 77, 78, 79, 86] {
        let mut bad = encoded.clone();
        bad[index] ^= 0xff;
        assert!(Directory::open(&bad).is_err(), "byte {index}");
    }
    assert!(Directory::open(&encoded[..encoded.len() - 1]).is_err());
    assert!(encode_directory(key(70), 9, &[descriptor, descriptor]).is_err());
    let directory = Directory::open(&encoded).unwrap();
    let mut bad_payload = payload.clone();
    bad_payload[2..4].copy_from_slice(&1u16.to_le_bytes());
    assert!(
        directory
            .offsets(5, |_, output| {
                output.copy_from_slice(&bad_payload);
                Ok(())
            })
            .is_err()
    );
    assert!(encode_offsets(70, &[1, 1]).is_err());
    assert!(encode_offsets(70, &[71]).is_err());
}

#[test]
fn inline_descriptor_rejects_invalid_offsets_and_physical_references() {
    let good = PageDescriptor::singleton(4, 70);
    let encoded = encode_directory(key(70), 1, &[good]).unwrap();
    let directory = Directory::open(&encoded).unwrap();
    let mask = directory
        .offsets(4, |_, _| panic!("inline read"))
        .unwrap()
        .unwrap();
    assert_eq!(mask[1], 1 << 5);
    for bad in [
        PageDescriptor::singleton(4, 0),
        PageDescriptor::singleton(4, 71),
        PageDescriptor { count: 2, ..good },
        PageDescriptor {
            extent: Extent {
                block: 1,
                ..good.extent
            },
            ..good
        },
        PageDescriptor {
            extent: Extent {
                len: 1,
                ..good.extent
            },
            ..good
        },
    ] {
        assert!(encode_directory(key(70), 1, &[bad]).is_err());
    }
}

#[test]
fn dense_tail_and_invalid_final_heap_coordinate_are_rejected() {
    let offsets: Vec<u16> = (1..=70).collect();
    let (kind, payload) = encode_offsets(70, &offsets).unwrap();
    assert_eq!(kind, ContainerKind::Dense);
    let descriptor = PageDescriptor {
        page: 1,
        kind,
        count: 70,
        extent: Extent {
            block: 3,
            offset: 0,
            len: payload.len() as u16,
        },
    };
    let encoded = encode_directory(key(70), 9, &[descriptor]).unwrap();
    let directory = Directory::open(&encoded).unwrap();
    let mut bad = payload;
    bad[8] |= 0b1000_0000;
    assert!(
        directory
            .offsets(1, |_, output| {
                output.copy_from_slice(&bad);
                Ok(())
            })
            .is_err()
    );
    let final_key = GroupKey::new(
        Generation::new(3).unwrap(),
        SegmentId::new(7).unwrap(),
        u32::MAX - 255,
        HeapLayout::new(70).unwrap(),
    )
    .unwrap();
    assert!(
        encode_directory(
            final_key,
            9,
            &[PageDescriptor {
                page: 255,
                ..descriptor
            }]
        )
        .is_err()
    );
}
