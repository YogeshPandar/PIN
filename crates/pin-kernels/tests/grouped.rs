use pin_kernels::BitmapOp;
use pin_kernels::grouped::{Pages, candidates, offsets};

#[test]
fn scalar_truth_tables_cover_every_page_and_offset_bit() {
    for bit in 0..512 {
        for left in [false, true] {
            for right in [false, true] {
                for live in [false, true] {
                    let make = |set| {
                        let mut words = [0; 8];
                        if set { words[bit / 64] = 1 << (bit % 64); }
                        words
                    };
                    for (op, expected) in [
                        (BitmapOp::Intersection, left && right && live),
                        (BitmapOp::Union, (left || right) && live),
                        (BitmapOp::Difference, left && !right && live),
                    ] {
                        assert_eq!(offsets(op, &make(left), &make(right), &make(live)), make(expected));
                    }
                }
            }
        }
    }
    for page in 0..256 {
        let mut mask = [0; 4];
        mask[page / 64] = 1 << (page % 64);
        assert_eq!(candidates(BitmapOp::Difference, &mask, &mask, &mask), mask);
        assert_eq!(candidates(BitmapOp::Intersection, &mask, &[0; 4], &mask), [0; 4]);
        assert_eq!(candidates(BitmapOp::Union, &mask, &[0; 4], &mask), mask);
    }
}

#[test]
fn page_iteration_is_ordered_and_fused_including_word_boundaries() {
    let mut pages = Pages::new([u64::MAX; 4]);
    for page in 0..=255u8 {
        assert_eq!(pages.next(), Some(page));
    }
    assert_eq!(pages.next(), None);
    assert_eq!(pages.next(), None);
    assert_eq!(Pages::new([0; 4]).next(), None);
    assert_eq!(Pages::new([1 << 63, 1, 0, 1 << 63]).collect::<Vec<_>>(), vec![63, 64, 255]);
}
