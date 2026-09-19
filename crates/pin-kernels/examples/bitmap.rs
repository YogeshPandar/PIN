// reproducible microbenchmark; shared-runner timings are not a release verdict.
// contracts: std::hint::black_box, std::time::Instant, docs/g6-performance.md.
#![forbid(unsafe_code)]

use pin_kernels::{BitmapOp, CpuMode, Kernels};
use std::hint::black_box;
use std::time::Instant;

fn main() {
    println!("sample,requested,selected,operation,words,word_offset,iterations,elapsed_ns,checksum");
    for sample in 0..5 {
        let modes = if sample % 2 == 0 {
            [CpuMode::Scalar, CpuMode::Avx2]
        } else {
            [CpuMode::Avx2, CpuMode::Scalar]
        };
        for words in [0usize, 1, 3, 4, 5, 8, 32, 128, 1024, 16384, 65536] {
            for offset in 0..4 {
                let left: Vec<_> = (0..words + 4)
                    .map(|i| (i as u64).wrapping_mul(0x9e3779b97f4a7c15))
                    .collect();
                let right: Vec<_> = left.iter().map(|value| value.rotate_left(17)).collect();
                let left = &left[offset..offset + words];
                let right = &right[3 - offset..3 - offset + words];
                for op in [BitmapOp::Intersection, BitmapOp::Union, BitmapOp::Difference] {
                    let reference: Vec<_> = left
                        .iter()
                        .zip(right)
                        .map(|(&a, &b)| match op {
                            BitmapOp::Intersection => a & b,
                            BitmapOp::Union => a | b,
                            BitmapOp::Difference => a & !b,
                        })
                        .collect();
                    for mode in modes {
                        let Ok(kernels) = Kernels::select(mode) else {
                            continue;
                        };
                        let mut storage = vec![0; words + 4];
                        let output = &mut storage[offset..offset + words];
                        kernels.combine(op, left, right, output).unwrap();
                        assert_eq!(output, reference.as_slice());
                        let iterations = (65536 / words.max(1)).max(16);
                        let start = Instant::now();
                        for _ in 0..iterations {
                            kernels
                                .combine(
                                    op,
                                    black_box(left),
                                    black_box(right),
                                    black_box(&mut *output),
                                )
                                .unwrap();
                            black_box(&*output);
                        }
                        let elapsed = start.elapsed().as_nanos();
                        assert_eq!(output, reference.as_slice());
                        let checksum = output.iter().fold(0u64, |sum, value| sum.wrapping_add(*value));
                        println!(
                            "{sample},{mode:?},{:?},{op:?},{words},{offset},{iterations},{elapsed},{checksum}",
                            kernels.mode()
                        );
                    }
                }
            }
        }
    }
}
