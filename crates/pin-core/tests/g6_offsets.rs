#![forbid(unsafe_code)]

use pin_core::codec::offsets::{Encoding, OffsetSet};
use pin_core::identity::HeapLayout;
use pin_kernels::{BitmapOp, CpuMode, Kernels};

fn verify(set: &OffsetSet, domain: u16) {
    let offsets: Vec<_> = set.iter().collect();
    let runs = offsets
        .iter()
        .enumerate()
        .filter(|&(i, value)| i == 0 || *value != offsets[i - 1] + 1)
        .count();
    let lengths = [
        6 + offsets.len() * 2,
        6 + usize::from(domain).div_ceil(8),
        8 + runs * 4,
    ];
    let encodings = [Encoding::Sparse, Encoding::Bitmap, Encoding::Runs];
    let mut best = 0;
    for (i, encoding) in encodings.into_iter().enumerate() {
        assert_eq!(set.encoded_len(encoding), lengths[i]);
        if lengths[i] < lengths[best] {
            best = i;
        }
        let mut bytes = [0; 1030];
        let len = set.encode_as(encoding, &mut bytes).unwrap();
        assert_eq!(len, lengths[i]);
        assert_eq!(
            OffsetSet::parse(&bytes[..len], HeapLayout::new(domain).unwrap()).unwrap(),
            *set
        );
    }
    assert_eq!(set.preferred_encoding(), encodings[best]);
}

#[test]
fn every_small_set_preserves_lengths_encodings_and_ties() {
    for domain in 1..=12 {
        for mask in 0u32..1 << domain {
            let mut set = OffsetSet::new(HeapLayout::new(domain).unwrap());
            for bit in 0..domain {
                if mask & (1 << bit) != 0 {
                    set.insert(bit + 1).unwrap();
                }
            }
            verify(&set, domain);
        }
    }
}

#[test]
fn every_domain_and_cross_word_run_matches_offset_reference() {
    for domain in 1..=512 {
        let layout = HeapLayout::new(domain).unwrap();
        for residue in 0..8 {
            let mut set = OffsetSet::new(layout);
            for offset in 1..=domain {
                if offset % 8 != residue {
                    set.insert(offset).unwrap();
                }
            }
            verify(&set, domain);
        }
        let mut dense = OffsetSet::new(layout);
        for offset in 1..=domain {
            dense.insert(offset).unwrap();
        }
        verify(&dense, domain);
    }
}

#[test]
fn opt_in_kernels_preserve_default_results_and_domain_guards() {
    for domain in [1, 7, 63, 64, 65, 127, 291, 511, 512] {
        let layout = HeapLayout::new(domain).unwrap();
        let mut left = OffsetSet::new(layout);
        let mut right = OffsetSet::new(layout);
        for offset in 1..=domain {
            if offset % 3 != 0 {
                left.insert(offset).unwrap();
            }
            if offset % 5 != 0 {
                right.insert(offset).unwrap();
            }
        }
        for mode in [CpuMode::Scalar, CpuMode::Auto, CpuMode::Avx2] {
            let Ok(kernels) = Kernels::select(mode) else {
                continue;
            };
            for (op, expected) in [
                (BitmapOp::Intersection, left.intersection(&right).unwrap()),
                (BitmapOp::Union, left.union(&right).unwrap()),
                (BitmapOp::Difference, left.difference(&right).unwrap()),
            ] {
                let result = left.combine_with(&right, op, kernels).unwrap();
                assert_eq!(result, expected);
                verify(&result, domain);
                let foreign =
                    OffsetSet::new(HeapLayout::new(if domain == 1 { 2 } else { 1 }).unwrap());
                assert!(left.combine_with(&foreign, op, kernels).is_err());
            }
        }
    }
}
