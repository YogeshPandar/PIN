//! One immutable owner map and one shared, clear-only liveness image per group.
//! Retirement edits private bytes only; the host must WAL-log before heap reuse.

use super::bitmap::{bitmap_len, encode_with};
use super::{Bitmap, BitmapKind, GROUP_PAGES, GroupKey, HEADER_BYTES, Header, MEMBERS};
use crate::codec::bytes::{Reader, Writer};
use crate::error::{Error, Result};
use crate::identity::{Incarnation, RootTid};
use pin_kernels::grouped::{OffsetMask, Pages};

const MEMBER_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Member {
    pub root: RootTid,
    pub incarnation: Incarnation,
}

/// Sorted unique coordinates; owner identity is stored once, not once per term.
#[derive(Clone, Copy)]
pub struct Members<'a> {
    bytes: &'a [u8],
    header: Header,
}

impl<'a> Members<'a> {
    /// Validates every membership record and the exact page summary.
    /// A coordinate cannot have two different occupants in the same segment.
    ///
    /// # Errors
    /// Rejects duplicate or unordered roots, invalid owners and foreign groups.
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        let header = Header::read(bytes)?;
        if header.kind != MEMBERS
            || header.count as usize > GROUP_PAGES * usize::from(header.key.layout.max_offset())
            || bytes.len() != HEADER_BYTES + header.count as usize * MEMBER_BYTES
        {
            return Err(Error::InvalidState);
        }
        let result = Self { bytes, header };
        let mut previous = None;
        let mut mask = [0; 4];
        for index in 0..result.len() {
            let member = result.get(index)?;
            validate_root(header.key, member.root)?;
            if previous.is_some_and(|previous| previous >= member.root) {
                return Err(Error::DuplicateDocument);
            }
            super::insert(&mut mask, member.root.block() as u8);
            previous = Some(member.root);
        }
        if mask != header.mask {
            return Err(Error::InvalidState);
        }
        Ok(result)
    }

    pub const fn key(&self) -> GroupKey {
        self.header.key
    }

    pub const fn len(&self) -> usize {
        self.header.count as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.header.count == 0
    }

    /// Reads one checked owner record without heap or relation I/O.
    ///
    /// # Errors
    /// Rejects an out-of-range member index.
    pub fn get(&self, index: usize) -> Result<Member> {
        if index >= self.len() {
            return Err(Error::InvalidParameters);
        }
        let start = HEADER_BYTES + index * MEMBER_BYTES;
        let mut reader = Reader::new(&self.bytes[start..start + MEMBER_BYTES]);
        let block = reader.u32()?;
        let offset = reader.u16()?;
        if reader.u16()? != 0 {
            return Err(Error::InvalidState);
        }
        let root = RootTid::new(block, offset, self.header.key.layout)
            .map_err(|_| Error::InvalidState)?;
        let incarnation = Incarnation::new(reader.u64()?).map_err(|_| Error::InvalidState)?;
        Ok(Member { root, incarnation })
    }

    /// Finds the single incarnation assigned to a coordinate.
    ///
    /// # Errors
    /// Propagates a malformed membership record.
    pub fn find(&self, root: RootTid) -> Result<Option<Member>> {
        let index = self.lower_bound(root.key())?;
        if index == self.len() {
            return Ok(None);
        }
        let member = self.get(index)?;
        Ok((member.root == root).then_some(member))
    }

    /// Builds initial shared liveness from the complete immutable membership.
    ///
    /// # Errors
    /// Rejects insufficient output; no allocation or owner-sized scratch is used.
    pub fn encode_liveness(&self, output: &mut [u8]) -> Result<usize> {
        encode_with(
            self.key(),
            BitmapKind::Liveness,
            self.header.mask,
            output,
            |page| self.page_offsets(page),
        )
    }

    pub(super) fn page_offsets(&self, page: u8) -> Result<OffsetMask> {
        let block = self.key().block(page)?;
        let mut index = self.lower_bound(u64::from(block) << 16)?;
        let mut mask = [0; 8];
        while index < self.len() {
            let member = self.get(index)?;
            if member.root.block() != block {
                break;
            }
            let bit = usize::from(member.root.offset() - 1);
            mask[bit / 64] |= 1 << (bit % 64);
            index += 1;
        }
        Ok(mask)
    }

    fn lower_bound(&self, key: u64) -> Result<usize> {
        let mut lo = 0;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get(mid)?.root.key() < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }
}

/// Encodes a complete, sorted membership into caller-owned bytes.
/// The host verifies canonical owner-to-root assignments under its writer lock.
/// Invalid input and insufficient capacity are rejected before writing.
///
/// # Errors
/// Rejects repeated roots, invalid owners, foreign groups and output limits.
pub fn encode_members(key: GroupKey, members: &[Member], output: &mut [u8]) -> Result<usize> {
    if members.len() > GROUP_PAGES * usize::from(key.layout.max_offset()) {
        return Err(Error::Limit("group members"));
    }
    let length = HEADER_BYTES + members.len() * MEMBER_BYTES;
    if length > output.len() {
        return Err(Error::Limit("group membership output"));
    }
    let mut previous = None;
    let mut mask = [0; 4];
    for member in members {
        validate_root(key, member.root)?;
        if previous.is_some_and(|previous| previous >= member.root) {
            return Err(Error::DuplicateDocument);
        }
        super::insert(&mut mask, member.root.block() as u8);
        previous = Some(member.root);
    }
    let mut writer = Writer::new(&mut output[..length]);
    Header {
        key,
        kind: MEMBERS,
        count: members.len() as u32,
        mask,
    }
    .write(&mut writer, length)?;
    for member in members {
        writer.u32(member.root.block())?;
        writer.u16(member.root.offset())?;
        writer.u16(0)?;
        writer.u64(member.incarnation.get())?;
    }
    Ok(length)
}

fn validate_root(key: GroupKey, root: RootTid) -> Result<()> {
    if root.block() & !255 != key.base || root.offset() > key.layout.max_offset() {
        return Err(Error::InvalidState);
    }
    Ok(())
}

/// A consistent private snapshot of generation membership and shared liveness.
/// Opening is a full liveness validation, separate from per-term query pruning.
pub struct SegmentGroup<'a> {
    members: Members<'a>,
    live: Bitmap<'a>,
}

impl<'a> SegmentGroup<'a> {
    /// Binds one liveness image to the exact immutable membership identity.
    ///
    /// # Errors
    /// Rejects foreign identities, changed geometry and live nonmembers.
    pub fn open(members: &'a [u8], live: &'a [u8]) -> Result<Self> {
        let members = Members::open(members)?;
        let live = Bitmap::open(live)?;
        if live.kind() != BitmapKind::Liveness
            || live.key() != members.key()
            || *live.pages() != members.header.mask
        {
            return Err(Error::InvalidState);
        }
        for page in Pages::new(members.header.mask) {
            let all = members.page_offsets(page)?;
            let current = live.offsets(page)?;
            if live.payload_bytes(page) != bitmap_len(members.key(), &all)?
                || current.iter().zip(all).any(|(&live, all)| live & !all != 0)
            {
                return Err(Error::InvalidState);
            }
        }
        Ok(Self { members, live })
    }

    pub const fn key(&self) -> GroupKey {
        self.members.key()
    }

    pub const fn members(&self) -> &Members<'a> {
        &self.members
    }

    pub(super) const fn live(&self) -> &Bitmap<'a> {
        &self.live
    }
}

/// Clears one matching incarnation in a private liveness image, idempotently.
/// The host must durably publish this image before PostgreSQL can reuse the TID.
/// This function does not acquire locks, write WAL or certify MVCC visibility.
///
/// # Errors
/// Rejects a foreign segment, unknown root or wrong owner before any mutation.
pub fn retire(members: &Members<'_>, live: &mut [u8], member: Member) -> Result<bool> {
    if members.find(member.root)? != Some(member) {
        return Err(Error::InvalidState);
    }
    let view = Bitmap::open(live)?;
    if view.kind() != BitmapKind::Liveness
        || view.key() != members.key()
        || *view.pages() != members.header.mask
    {
        return Err(Error::InvalidState);
    }
    let page = member.root.block() as u8;
    view.offsets(page)?;
    let all = members.page_offsets(page)?;
    let (start, len) = view.span(page).ok_or(Error::InvalidState)?;
    if len != bitmap_len(members.key(), &all)? {
        return Err(Error::InvalidState);
    }
    let bit = usize::from(member.root.offset() - 1);
    let byte = start + bit / 8;
    let mask = 1u8 << (bit % 8);
    let changed = live[byte] & mask != 0;
    live[byte] &= !mask;
    Ok(changed)
}
