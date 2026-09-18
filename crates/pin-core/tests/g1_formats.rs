use pin_core::codec::dictionary::{self, Dictionary, DictionaryLimits, TermId};
use pin_core::codec::offsets::{Encoding, OffsetSet};
use pin_core::codec::records::{self, DocumentRecord, Manifest, Publication, Source};
use pin_core::identity::{Generation, HeapLayout, Incarnation, RootTid, SegmentId};
use std::collections::BTreeSet;

const ENCODINGS: [Encoding; 3] = [Encoding::Sparse, Encoding::Bitmap, Encoding::Runs];

fn offsets(domain: u16, seed: u32) -> OffsetSet {
    let mut set = OffsetSet::new(HeapLayout::new(domain).unwrap());
    for offset in 1..=domain {
        if (u32::from(offset) * 17 + seed) % 11 < seed % 12 {
            set.insert(offset).unwrap();
        }
    }
    set
}

#[test]
fn mixed_container_operations_match_independent_sets() {
    for domain in [1, 7, 8, 9, 63, 64, 65, 291, 511, 512] {
        let layout = HeapLayout::new(domain).unwrap();
        for left_seed in 0..12 {
            let left = offsets(domain, left_seed);
            let left_ref: BTreeSet<_> = left.iter().collect();
            for right_seed in 0..12 {
                let right = offsets(domain, right_seed);
                let right_ref: BTreeSet<_> = right.iter().collect();
                for le in ENCODINGS {
                    for re in ENCODINGS {
                        let mut lb = [0; 1030];
                        let mut rb = [0; 1030];
                        let ll = left.encode_as(le, &mut lb).unwrap();
                        let rl = right.encode_as(re, &mut rb).unwrap();
                        let l = OffsetSet::parse(&lb[..ll], layout).unwrap();
                        let r = OffsetSet::parse(&rb[..rl], layout).unwrap();
                        assert_eq!(
                            l.intersection(&r).unwrap().iter().collect::<Vec<_>>(),
                            left_ref
                                .intersection(&right_ref)
                                .copied()
                                .collect::<Vec<_>>()
                        );
                        assert_eq!(
                            l.union(&r).unwrap().iter().collect::<Vec<_>>(),
                            left_ref.union(&right_ref).copied().collect::<Vec<_>>()
                        );
                        assert_eq!(
                            l.difference(&r).unwrap().iter().collect::<Vec<_>>(),
                            left_ref.difference(&right_ref).copied().collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn offset_golden_tails_and_truncations() {
    let layout = HeapLayout::new(9).unwrap();
    let mut set = OffsetSet::new(layout);
    set.insert(1).unwrap();
    set.insert(9).unwrap();
    let mut bytes = [0; 1030];
    let len = set.encode_as(Encoding::Bitmap, &mut bytes).unwrap();
    assert_eq!(&bytes[..len], &[9, 0, 2, 0, 1, 0, 1, 1]);
    for cut in 0..len {
        assert!(OffsetSet::parse(&bytes[..cut], layout).is_err());
    }
    bytes[len - 1] |= 2;
    assert!(OffsetSet::parse(&bytes[..len], layout).is_err());
    assert!(set.insert(0).is_err());
    assert!(set.insert(10).is_err());
    assert!(
        set.union(&OffsetSet::new(HeapLayout::new(8).unwrap()))
            .is_err()
    );
    let mut iter = set.iter();
    assert_eq!(iter.next(), Some(1));
    assert_eq!(iter.next(), Some(9));
    for _ in 0..1000 {
        assert_eq!(iter.next(), None);
    }
    for encoding in ENCODINGS {
        let len = set.encode_as(encoding, &mut bytes).unwrap();
        assert_eq!(len, set.encoded_len(encoding));
        assert_eq!(OffsetSet::parse(&bytes[..len], layout).unwrap(), set);
        for cut in 0..len {
            assert!(OffsetSet::parse(&bytes[..cut], layout).is_err());
        }
    }
}

#[test]
fn document_records_preserve_incarnation_and_reject_invalid_states() {
    let layout = HeapLayout::new(128).unwrap();
    let record = DocumentRecord {
        segment: SegmentId::new(1).unwrap(),
        incarnation: Incarnation::new(2).unwrap(),
        root: RootTid::new(42, 7, layout).unwrap(),
        token_count: 3,
        profile: 1,
        publication: Publication::Published,
        live: true,
    };
    let mut bytes = [0; DocumentRecord::ENCODED_BYTES];
    record.encode(&mut bytes).unwrap();
    assert_eq!(
        &bytes[..16],
        b"PIN1\x01\x00\x01\x00\x00\x00\x00\x00\x24\x00\x00\x00"
    );
    assert_eq!(&bytes[16..24], &1u64.to_le_bytes());
    assert_eq!(&bytes[24..32], &2u64.to_le_bytes());
    assert_eq!(DocumentRecord::parse(&bytes, layout).unwrap(), record);
    for cut in 0..bytes.len() {
        assert!(DocumentRecord::parse(&bytes[..cut], layout).is_err());
    }
    assert!(
        DocumentRecord {
            publication: Publication::Allocated,
            ..record
        }
        .encode(&mut bytes)
        .is_err()
    );
    let replacement = DocumentRecord {
        incarnation: Incarnation::new(3).unwrap(),
        ..record
    };
    assert_ne!(record, replacement);
    for (offset, bad) in [
        (0, 0),
        (5, 1),
        (6, 2),
        (8, 1),
        (16, 0),
        (36, 0),
        (38, 1),
        (48, 255),
        (49, 2),
        (50, 1),
    ] {
        record.encode(&mut bytes).unwrap();
        bytes[offset] = bad;
        assert!(
            DocumentRecord::parse(&bytes, layout).is_err(),
            "offset {offset}"
        );
    }
}

#[test]
fn values_distinguish_null_empty_and_exact_unicode_bytes() {
    let mut bytes = [0; 256];
    for value in [None, Some(""), Some("a\0b"), Some("café e\u{301} 東京")] {
        let len = records::encode_value(value, &mut bytes, 256).unwrap();
        assert_eq!(records::decode_value(&bytes[..len], 256).unwrap(), value);
        for cut in 0..len {
            assert!(records::decode_value(&bytes[..cut], 256).is_err());
        }
        assert!(records::decode_value(&bytes[..len + 1], 256).is_err());
    }
    let len = records::encode_value(Some("a"), &mut bytes, 256).unwrap();
    bytes[len - 1] = 255;
    assert!(records::decode_value(&bytes[..len], 256).is_err());
    assert!(records::encode_value(Some(""), &mut bytes, 20).is_err());
}

#[test]
fn dictionary_exact_lookup_prefix_bounds_and_untrusted_offsets() {
    let limits = DictionaryLimits {
        max_bytes: 1024,
        max_terms: 10,
        max_term_bytes: 32,
    };
    let mut bytes = [0; 1024];
    let terms = ["a", "alpha", "alpine", "βeta"];
    let len = dictionary::encode(7, &terms, &mut bytes, limits).unwrap();
    let dict = Dictionary::parse(&bytes[..len], limits).unwrap();
    assert_eq!(dict.profile(), 7);
    assert_eq!(dict.prefix("al", 2).unwrap(), 1..3);
    assert!(dict.prefix("al", 1).is_err());
    assert_eq!(dict.prefix("z", 0).unwrap(), 3..3);
    assert_eq!(dict.prefix("β", 1).unwrap(), 3..4);
    assert_eq!(dict.prefix("", 4).unwrap(), 0..4);
    assert!(dict.prefix("", 3).is_err());
    assert!(dict.prefix("β", 0).is_err());
    assert_eq!(dict.prefix("東京", 0).unwrap(), 4..4);
    assert_eq!(dict.lookup("alp").unwrap(), None);
    for (id, &term) in terms.iter().enumerate() {
        assert_eq!(dict.lookup(term).unwrap(), Some(TermId(id as u32)));
        assert_eq!(dict.term(TermId(id as u32)).unwrap(), term);
    }
    assert!(dict.term(TermId(4)).is_err());
    for cut in 0..len {
        assert!(Dictionary::parse(&bytes[..cut], limits).is_err());
    }
    bytes[24] = 1;
    assert!(Dictionary::parse(&bytes[..len], limits).is_err());
    assert!(dictionary::encode(7, &["b", "a"], &mut bytes, limits).is_err());
    assert!(dictionary::encode(7, &["a", "a"], &mut bytes, limits).is_err());
    assert!(dictionary::encode(7, &[""], &mut bytes, limits).is_err());
    let len = dictionary::encode(7, &[], &mut bytes, limits).unwrap();
    assert!(Dictionary::parse(&bytes[..len], limits).unwrap().is_empty());
}

#[test]
fn manifest_validates_unique_sorted_sources_and_reserved_bytes() {
    let sources = [
        Source {
            id: SegmentId::new(1).unwrap(),
            sealed: true,
        },
        Source {
            id: SegmentId::new(3).unwrap(),
            sealed: false,
        },
    ];
    let mut bytes = [0; 256];
    let generation = Generation::new(4).unwrap();
    let len = records::encode_manifest(generation, &sources, &mut bytes, 2).unwrap();
    let manifest = Manifest::parse(&bytes[..len], 2, 256).unwrap();
    assert_eq!(manifest.generation, generation);
    for (index, source) in sources.iter().enumerate() {
        assert_eq!(&manifest.source(index as u32).unwrap(), source);
    }
    assert!(manifest.source(2).is_err());
    for cut in 0..len {
        assert!(Manifest::parse(&bytes[..cut], 2, 256).is_err());
    }
    assert!(Manifest::parse(&bytes[..len], 1, 256).is_err());
    bytes[37] = 1;
    assert!(Manifest::parse(&bytes[..len], 2, 256).is_err());
    assert!(
        records::encode_manifest(generation, &[sources[0], sources[0]], &mut bytes, 2).is_err()
    );
}

#[test]
fn deterministic_malformed_payloads_never_panic() {
    let layout = HeapLayout::new(291).unwrap();
    let limits = DictionaryLimits {
        max_bytes: 256,
        max_terms: 64,
        max_term_bytes: 32,
    };
    let mut bytes = [0; 256];
    let mut seed = 0x1234abcd_u32;
    for len in 0..256 {
        for byte in bytes.iter_mut().take(len) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *byte = seed as u8;
        }
        let _ = OffsetSet::parse(&bytes[..len], layout);
        let _ = DocumentRecord::parse(&bytes[..len], layout);
        let _ = records::decode_value(&bytes[..len], 256);
        let _ = Dictionary::parse(&bytes[..len], limits);
        let _ = Manifest::parse(&bytes[..len], 16, 256);
    }
}

#[test]
fn prefix_intervals_match_a_linear_byte_order_oracle() {
    let mut terms = [
        "a",
        "a\0",
        "ab",
        "abe",
        "β",
        "βeta",
        "βζ",
        "東京",
        "東海道",
        "\u{10ffff}",
        "\u{10ffff}a",
    ];
    terms.sort_unstable();
    let limits = DictionaryLimits {
        max_bytes: 4096,
        max_terms: 32,
        max_term_bytes: 32,
    };
    let mut bytes = [0; 4096];
    let len = dictionary::encode(7, &terms, &mut bytes, limits).unwrap();
    let dictionary = Dictionary::parse(&bytes[..len], limits).unwrap();
    for prefix in [
        "",
        "!",
        "a",
        "a\0",
        "abc",
        "abe",
        "z",
        "β",
        "βe",
        "東",
        "東京",
        "\u{10ffff}",
        "\u{10ffff}z",
    ] {
        let begin = terms
            .iter()
            .position(|term| *term >= prefix)
            .unwrap_or(terms.len());
        let count = terms[begin..]
            .iter()
            .take_while(|term| term.starts_with(prefix))
            .count();
        for limit in 0..=terms.len() {
            let actual = dictionary.prefix(prefix, limit as u32);
            if count > limit {
                assert_eq!(
                    actual.unwrap_err().kind,
                    pin_core::codec::ErrorKind::LimitExceeded
                );
            } else {
                assert_eq!(actual.unwrap(), begin as u32..(begin + count) as u32);
            }
        }
    }
}
