//! Scalar page pruning and live offset operations over fixed caller-owned masks.
//! Difference cannot subtract page summaries: distinct offsets may share a page.
#![forbid(unsafe_code)]

use crate::BitmapOp;

pub const PAGE_WORDS: usize = 4;
pub const OFFSET_WORDS: usize = 8;
pub type PageMask = [u64; PAGE_WORDS];
pub type OffsetMask = [u64; OFFSET_WORDS];

/// Computes conservative candidate pages without inspecting offset payloads.
/// Live page summaries may contain pages whose offsets have all been retired.
pub fn candidates(
    operation: BitmapOp,
    left: &PageMask,
    right: &PageMask,
    live: &PageMask,
) -> PageMask {
    core::array::from_fn(|index| {
        let value = match operation {
            BitmapOp::Intersection => left[index] & right[index],
            BitmapOp::Union => left[index] | right[index],
            BitmapOp::Difference => left[index],
        };
        value & live[index]
    })
}

/// Computes exact offset membership within one validated generation domain.
/// The caller establishes identity equality and masks every unused tail bit.
pub fn offsets(
    operation: BitmapOp,
    left: &OffsetMask,
    right: &OffsetMask,
    live: &OffsetMask,
) -> OffsetMask {
    core::array::from_fn(|index| {
        let value = match operation {
            BitmapOp::Intersection => left[index] & right[index],
            BitmapOp::Union => left[index] | right[index],
            BitmapOp::Difference => left[index] & !right[index],
        };
        value & live[index]
    })
}

/// Visits set bits in heap-page order with no allocation or trailing zero scan.
#[derive(Clone, Debug)]
pub struct Pages {
    mask: PageMask,
    word: usize,
}

impl Pages {
    pub const fn new(mask: PageMask) -> Self {
        Self { mask, word: 0 }
    }
}

impl Iterator for Pages {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        while self.word < PAGE_WORDS {
            let pending = self.mask[self.word];
            if pending != 0 {
                let bit = pending.trailing_zeros() as usize;
                self.mask[self.word] = pending & (pending - 1);
                return Some((self.word * 64 + bit) as u8);
            }
            self.word += 1;
        }
        None
    }
}

impl core::iter::FusedIterator for Pages {}
