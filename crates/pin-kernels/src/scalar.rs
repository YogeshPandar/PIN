// portable reference; operation selection is constant outside the word loop.
// contract: https://doc.rust-lang.org/std/primitive.u64.html

pub(super) fn combine<const UNION: bool, const DIFFERENCE: bool>(
    left: &[u64],
    right: &[u64],
    output: &mut [u64],
) {
    for ((&left, &right), output) in left.iter().zip(right).zip(output) {
        *output = if UNION {
            left | right
        } else if DIFFERENCE {
            left & !right
        } else {
            left & right
        };
    }
}
