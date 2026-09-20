// private avx2 implementation; callers establish runtime support and equal lengths.
// full chunks belong to live slices; tails never use a wide load or store.
// contracts: docs/g6-api-evidence.md and core::arch::x86_64.

use std::arch::x86_64::{
    _mm256_and_si256, _mm256_andnot_si256, _mm256_loadu_si256, _mm256_or_si256, _mm256_storeu_si256,
};

#[target_feature(enable = "avx2")]
pub(super) unsafe fn combine<const UNION: bool, const DIFFERENCE: bool>(
    left: &[u64],
    right: &[u64],
    output: &mut [u64],
) {
    let (left, left_tail) = left.as_chunks::<4>();
    let (right, right_tail) = right.as_chunks::<4>();
    let (output, output_tail) = output.as_chunks_mut::<4>();
    for ((left, right), output) in left.iter().zip(right).zip(output) {
        // safety: each initialized chunk contains four u64s in one live allocation.
        // loadu accepts every alignment of these borrowed 32-byte ranges.
        let (left, right) = unsafe {
            (
                _mm256_loadu_si256(left.as_ptr().cast()),
                _mm256_loadu_si256(right.as_ptr().cast()),
            )
        };
        let value = if UNION {
            _mm256_or_si256(left, right)
        } else if DIFFERENCE {
            _mm256_andnot_si256(right, left)
        } else {
            _mm256_and_si256(left, right)
        };
        // safety: the exclusive chunk supplies all 32 writable bytes and cannot
        // overlap either input; storeu has no vector-alignment requirement.
        unsafe { _mm256_storeu_si256(output.as_mut_ptr().cast(), value) };
    }
    super::scalar::combine::<UNION, DIFFERENCE>(left_tail, right_tail, output_tail);
}
