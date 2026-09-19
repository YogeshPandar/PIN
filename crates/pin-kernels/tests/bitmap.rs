#![forbid(unsafe_code)]

use pin_kernels::{BitmapOp, CpuMode, KernelError, Kernels};

const OPS: [BitmapOp; 3] = [
    BitmapOp::Intersection,
    BitmapOp::Union,
    BitmapOp::Difference,
];

fn expected(op: BitmapOp, left: u64, right: u64) -> u64 {
    match op {
        BitmapOp::Intersection => left & right,
        BitmapOp::Union => left | right,
        BitmapOp::Difference => left & !right,
    }
}

#[test]
fn modes_and_every_length_mismatch_fail_before_writes() {
    assert_eq!(Kernels::scalar().mode(), CpuMode::Scalar);
    let auto = Kernels::select(CpuMode::Auto).unwrap();
    assert_ne!(auto.mode(), CpuMode::Auto);
    match Kernels::select(CpuMode::Avx2) {
        Ok(kernel) => assert_eq!(kernel.mode(), CpuMode::Avx2),
        Err(error) => {
            assert_eq!(error, KernelError::UnsupportedCpu);
            assert_eq!(auto.mode(), CpuMode::Scalar);
        }
    }
    for mode in [CpuMode::Scalar, CpuMode::Auto, CpuMode::Avx2] {
        let Ok(kernel) = Kernels::select(mode) else {
            continue;
        };
        for op in OPS {
            for left_len in 0..9 {
                for right_len in 0..9 {
                    for out_len in 0..9 {
                        let mut output = [u64::MAX; 9];
                        let result = kernel.combine(
                            op,
                            &[3; 9][..left_len],
                            &[5; 9][..right_len],
                            &mut output[..out_len],
                        );
                        if left_len != right_len || left_len != out_len {
                            assert_eq!(result, Err(KernelError::LengthMismatch));
                            assert_eq!(output, [u64::MAX; 9]);
                        } else {
                            result.unwrap();
                            assert!(output[out_len..].iter().all(|&value| value == u64::MAX));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn all_vector_alignments_tails_and_densities_match_independent_words() {
    let lengths: Vec<usize> = if cfg!(miri) {
        (0..12).collect()
    } else {
        (0..132).collect()
    };
    for mode in [CpuMode::Scalar, CpuMode::Auto, CpuMode::Avx2] {
        let Ok(kernel) = Kernels::select(mode) else {
            continue;
        };
        for &len in &lengths {
            for start in 0..4 {
                let left: Vec<_> = (0..len + 4)
                    .map(|i| 0x9e3779b97f4a7c15u64.wrapping_mul(i as u64))
                    .collect();
                for right_start in 0..4 {
                    for out_start in 0..4 {
                        for mask in [0, 1, 0x5555555555555555, u64::MAX] {
                            let right: Vec<_> =
                                (0..len + 4).map(|i| mask ^ (1u64 << (i % 64))).collect();
                            for op in OPS {
                                let mut output = vec![0xdeadbeef; len + 8];
                                kernel
                                    .combine(
                                        op,
                                        &left[start..start + len],
                                        &right[right_start..right_start + len],
                                        &mut output[out_start..out_start + len],
                                    )
                                    .unwrap();
                                for i in 0..len {
                                    assert_eq!(
                                        output[out_start + i],
                                        expected(op, left[start + i], right[right_start + i])
                                    );
                                }
                                assert!(
                                    output[..out_start]
                                        .iter()
                                        .chain(&output[out_start + len..])
                                        .all(|&value| value == 0xdeadbeef)
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn exact_allocations_and_identical_inputs_include_every_tail() {
    for mode in [CpuMode::Scalar, CpuMode::Auto, CpuMode::Avx2] {
        let Ok(kernel) = Kernels::select(mode) else {
            continue;
        };
        for len in 0..65 {
            let input = vec![u64::MAX; len].into_boxed_slice();
            for op in OPS {
                let mut output = vec![0; len].into_boxed_slice();
                kernel.combine(op, &input, &input, &mut output).unwrap();
                assert!(
                    output
                        .iter()
                        .all(|&value| value == expected(op, u64::MAX, u64::MAX))
                );
            }
        }
    }
}
