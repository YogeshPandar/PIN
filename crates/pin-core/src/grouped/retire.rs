//! batch retirement by a stable physical-death predicate, not snapshot visibility.
//! edits are staged in bounded scratch before modifying a private liveness image.

use super::{Bitmap, BitmapKind, GroupKey};
use crate::error::{Error, Result};
use crate::identity::RootTid;
use pin_kernels::grouped::Pages;

const MAX_BYTES: usize = 72 + 256 * 68;

/// clears all callback-approved roots in one exact generation's private image.
/// the callback must mean globally removable, not invisible to one snapshot.
/// the host holds the writer interlock and WAL-publishes before heap slot reuse.
///
/// # errors
/// rejects foreign identities, corrupt payloads and callback errors before mutation.
/// the function performs no allocation, persistence or visibility certification.
pub fn retire_roots(
    bytes: &mut [u8],
    key: GroupKey,
    mut removable: impl FnMut(RootTid) -> Result<bool>,
) -> Result<u32> {
    let view = Bitmap::open(bytes)?;
    if view.kind() != BitmapKind::Liveness || view.key() != key {
        return Err(Error::InvalidState);
    }
    let mut clear = [0u8; MAX_BYTES];
    let mut removed = 0;
    for page in Pages::new(*view.pages()) {
        let offsets = view.offsets(page)?;
        let (start, _) = view.span(page).ok_or(Error::InvalidState)?;
        for (word, &value) in offsets.iter().enumerate() {
            let mut pending = value;
            while pending != 0 {
                let bit = word * 64 + pending.trailing_zeros() as usize;
                pending &= pending - 1;
                let root = RootTid::new(key.block(page)?, (bit + 1) as u16, key.layout())
                    .map_err(|_| Error::InvalidState)?;
                if removable(root)? {
                    clear[start + bit / 8] |= 1 << (bit % 8);
                    removed += 1;
                }
            }
        }
    }
    for (byte, clear) in bytes.iter_mut().zip(clear) {
        *byte &= !clear;
    }
    Ok(removed)
}
