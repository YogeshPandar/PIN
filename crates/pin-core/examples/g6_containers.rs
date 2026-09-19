// compares encoded-size selection with an independent per-offset run counter.
// contracts: std::hint::black_box, std::time::Instant, docs/g6-performance.md.
#![forbid(unsafe_code)]

use pin_core::codec::offsets::{Encoding, OffsetSet};
use pin_core::identity::HeapLayout;
use std::hint::black_box;
use std::time::Instant;

fn reference(set: &OffsetSet, domain: u16) -> Encoding {
    let mut previous = 0;
    let mut runs = 0;
    for offset in set.iter() {
        if previous == 0 || offset != previous + 1 {
            runs += 1;
        }
        previous = offset;
    }
    let mut best = (6 + usize::from(set.len()) * 2, Encoding::Sparse);
    for choice in [
        (6 + usize::from(domain).div_ceil(8), Encoding::Bitmap),
        (8 + runs * 4, Encoding::Runs),
    ] {
        if choice.0 < best.0 {
            best = choice;
        }
    }
    best.1
}

fn main() {
    println!("sample,path,domain,pattern,cardinality,encoding,encoded_bytes,iterations,elapsed_ns");
    for sample in 0..5 {
        for domain in [1, 64, 128, 291, 512] {
            for pattern in 0..6 {
                let mut set = OffsetSet::new(HeapLayout::new(domain).unwrap());
                for offset in 1..=domain {
                    let include = match pattern {
                        0 => false,
                        1 => offset == domain,
                        2 => offset >= domain / 3,
                        3 => true,
                        4 => offset % 2 == 0,
                        _ => (u32::from(offset) * 17 + 5) % 11 < 7,
                    };
                    if include {
                        set.insert(offset).unwrap();
                    }
                }
                let expected = reference(&set, domain);
                assert_eq!(set.preferred_encoding(), expected);
                let paths = if sample % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                };
                for per_offset in paths {
                    let start = Instant::now();
                    for _ in 0..10_000 {
                        black_box(if per_offset {
                            reference(black_box(&set), domain)
                        } else {
                            black_box(&set).preferred_encoding()
                        });
                    }
                    let elapsed = start.elapsed().as_nanos();
                    let path = if per_offset {
                        "offset-reference"
                    } else {
                        "word"
                    };
                    println!(
                        "{sample},{path},{domain},{pattern},{},{expected:?},{},10000,{elapsed}",
                        set.len(),
                        set.encoded_len(expected)
                    );
                }
            }
        }
    }
}
