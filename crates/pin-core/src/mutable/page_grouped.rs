//! checked physical envelopes for grouped records and immutable catalog nodes.
//! legacy metadata remains readable; grouped state occupies an optional tail.

use super::{BUCKETS, HEADER, META_HEADER, NO_BLOCK, OwnerRef, Page, PageKind, RewritePhase};
use crate::codec::bytes::{Reader, Writer};
use crate::error::{Error, Result};
use crate::identity::{HeapLayout, SegmentId};

pub(super) const META_LEGACY_BYTES: usize = META_HEADER + BUCKETS * 8;
pub(super) const META_GROUPED_BYTES: usize = META_LEGACY_BYTES + 72;
const GROUP_HEADER: usize = HEADER + 16;
const DATA_HEADER: usize = GROUP_HEADER + 24;
const NODE_HEADER: usize = GROUP_HEADER + 8;
const ENTRY_BYTES: usize = 80;
pub const GROUP_DATA_BYTES: usize = super::CAPACITY - DATA_HEADER;
pub const CATALOG_ENTRIES: usize = (super::CAPACITY - NODE_HEADER) / ENTRY_BYTES;
pub const MAX_CATALOG_LEVEL: u8 = 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupSnapshot {
    pub id: SegmentId,
    pub head: u32,
    pub tail: u32,
    pub root: u32,
    pub after: Option<OwnerRef>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupJournal {
    pub id: SegmentId,
    pub head: u32,
    pub tail: u32,
    pub phase: RewritePhase,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GroupState {
    pub active: Option<GroupSnapshot>,
    pub journal: Option<GroupJournal>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum GroupPageKind {
    Members = 1,
    Liveness = 2,
    Posting = 3,
    Leaf = 4,
    Branch = 5,
}

/// fixed-width catalog keys sort by term identity, then heap group base.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogEntry {
    pub key: [u64; 2],
    pub value: [u8; 64],
}

/// borrows only a checked private page image, never a host buffer.
pub(crate) struct GroupNode<'a> {
    bytes: &'a [u8],
    pub level: u8,
    pub count: u16,
}

impl GroupNode<'_> {
    pub fn key(&self, index: u16) -> Result<[u64; 2]> {
        if index >= self.count {
            return Err(Error::InvalidParameters);
        }
        let start = usize::from(index) * ENTRY_BYTES;
        let mut reader = Reader::new(&self.bytes[start..start + 16]);
        Ok([reader.u64()?, reader.u64()?])
    }

    pub fn entry(&self, index: u16) -> Result<CatalogEntry> {
        let key = self.key(index)?;
        let start = usize::from(index) * ENTRY_BYTES + 16;
        let mut value = [0; 64];
        value.copy_from_slice(&self.bytes[start..start + 64]);
        Ok(CatalogEntry { key, value })
    }
}

impl Default for CatalogEntry {
    fn default() -> Self {
        Self {
            key: [0; 2],
            value: [0; 64],
        }
    }
}

pub struct GroupData<'a> {
    pub id: SegmentId,
    pub kind: GroupPageKind,
    pub key: [u64; 2],
    pub total: u32,
    pub offset: u32,
    pub bytes: &'a [u8],
}

fn pair(head: u32, tail: u32) -> bool {
    super::posting_pair_valid(head, tail)
}

fn valid_key(key: [u64; 2]) -> bool {
    key[1] <= u64::from(u32::MAX - 255)
        && key[1] & 255 == 0
        && (key[0] == 0
            || (key[0] >> 16 > 0
                && key[0] >> 16 < u64::from(NO_BLOCK)
                && key[0] as u16 >= HEADER as u16
                && usize::from(key[0] as u16) < super::CAPACITY))
}

impl GroupState {
    fn validate(self) -> Result<()> {
        if let Some(active) = self.active
            && (!pair(active.head, active.tail)
                || (active.head == NO_BLOCK) != (active.root == NO_BLOCK)
                || active.root != active.tail
                || (active.root != NO_BLOCK && !super::block_valid(active.root))
                || active
                    .after
                    .is_some_and(|owner| owner.incarnation.get() >= active.id.get()))
        {
            return Err(Error::InvalidState);
        }
        if let Some(journal) = self.journal
            && (!pair(journal.head, journal.tail)
                || (journal.phase == RewritePhase::Retiring
                    && (journal.head == NO_BLOCK || self.active.is_none()))
                || self.active.is_some_and(|active| {
                    let ordered = match journal.phase {
                        RewritePhase::Building => journal.id > active.id,
                        RewritePhase::Retiring => journal.id < active.id,
                    };
                    !ordered
                        || (journal.head != NO_BLOCK
                            && [active.head, active.tail].contains(&journal.head))
                        || (journal.tail != NO_BLOCK
                            && [active.head, active.tail].contains(&journal.tail))
                }))
        {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
}

impl Page {
    /// tests a captured allocation fence, not transaction visibility.
    pub(crate) fn grouped_has_delta(&self, snapshot: GroupSnapshot) -> Result<bool> {
        self.require(PageKind::Meta)?;
        let next = self.u64(32)?;
        if next <= snapshot.id.get() {
            return Err(Error::InvalidState);
        }
        Ok(next - 1 > snapshot.id.get())
    }

    /// reads either legacy metadata or the versioned grouped state tail.
    pub fn grouped_state(&self) -> Result<GroupState> {
        self.require(PageKind::Meta)?;
        if self.len == META_LEGACY_BYTES {
            return Ok(GroupState::default());
        }
        if self.len != META_GROUPED_BYTES {
            return Err(Error::InvalidState);
        }
        let mut reader = Reader::new(&self.bytes[META_LEGACY_BYTES..self.len]);
        if reader.take(4)? != b"PG09" || reader.u16()? != 1 || reader.u16()? != 0 {
            return Err(Error::InvalidState);
        }
        let id = reader.u64()?;
        let head = reader.u32()?;
        let tail = reader.u32()?;
        let root = reader.u32()?;
        if reader.u32()? != 0 {
            return Err(Error::InvalidState);
        }
        let owner = reader.take(16)?;
        let after = if owner == [0; 16] {
            None
        } else {
            Some(OwnerRef::read(&mut Reader::new(owner))?)
        };
        let active = if id == 0 {
            if head != NO_BLOCK || tail != NO_BLOCK || root != NO_BLOCK || after.is_some() {
                return Err(Error::InvalidState);
            }
            None
        } else {
            Some(GroupSnapshot {
                id: SegmentId::new(id).map_err(|_| Error::InvalidState)?,
                head,
                tail,
                root,
                after,
            })
        };
        let id = reader.u64()?;
        let head = reader.u32()?;
        let tail = reader.u32()?;
        let phase = reader.u32()?;
        if reader.u32()? != 0 {
            return Err(Error::InvalidState);
        }
        reader.finish()?;
        let journal = if id == 0 {
            if head != NO_BLOCK || tail != NO_BLOCK || phase != 0 {
                return Err(Error::InvalidState);
            }
            None
        } else {
            Some(GroupJournal {
                id: SegmentId::new(id).map_err(|_| Error::InvalidState)?,
                head,
                tail,
                phase: match phase {
                    1 => RewritePhase::Building,
                    2 => RewritePhase::Retiring,
                    _ => return Err(Error::InvalidState),
                },
            })
        };
        let state = GroupState { active, journal };
        state.validate()?;
        let next = self.u64(32)?;
        if state.active.is_some_and(|active| active.id.get() >= next)
            || state
                .journal
                .is_some_and(|journal| journal.id.get() >= next)
        {
            return Err(Error::InvalidState);
        }
        Ok(state)
    }

    /// updates a private metadata image; the host publishes it through WAL.
    pub fn set_grouped_state(&mut self, state: GroupState) -> Result<()> {
        self.require(PageKind::Meta)?;
        state.validate()?;
        let next = self.u64(32)?;
        if state.active.is_some_and(|active| active.id.get() >= next)
            || state
                .journal
                .is_some_and(|journal| journal.id.get() >= next)
        {
            return Err(Error::InvalidState);
        }
        let mut bytes = [0u8; 72];
        let mut writer = Writer::new(&mut bytes);
        writer.put(b"PG09")?;
        writer.u16(1)?;
        writer.u16(0)?;
        writer.u64(state.active.map_or(0, |active| active.id.get()))?;
        writer.u32(state.active.map_or(NO_BLOCK, |active| active.head))?;
        writer.u32(state.active.map_or(NO_BLOCK, |active| active.tail))?;
        writer.u32(state.active.map_or(NO_BLOCK, |active| active.root))?;
        writer.u32(0)?;
        if let Some(owner) = state.active.and_then(|active| active.after) {
            owner.write(&mut writer)?;
        } else {
            writer.put(&[0; 16])?;
        }
        writer.u64(state.journal.map_or(0, |journal| journal.id.get()))?;
        writer.u32(state.journal.map_or(NO_BLOCK, |journal| journal.head))?;
        writer.u32(state.journal.map_or(NO_BLOCK, |journal| journal.tail))?;
        writer.u32(state.journal.map_or(0, |journal| journal.phase as u32))?;
        writer.u32(0)?;
        self.bytes[META_LEGACY_BYTES..META_GROUPED_BYTES].copy_from_slice(&bytes);
        self.len = META_GROUPED_BYTES;
        Ok(())
    }

    fn new_grouped(block: u32, id: SegmentId, kind: GroupPageKind) -> Result<Self> {
        let mut page = Self::new(block, PageKind::Grouped)?;
        let mut writer = Writer::new(&mut page.bytes[HEADER..]);
        writer.put(b"G9PG")?;
        writer.u16(1)?;
        writer.u8(kind as u8)?;
        writer.u8(0)?;
        writer.u64(id.get())?;
        page.len = GROUP_HEADER;
        Ok(page)
    }

    pub fn group_identity(&self) -> Result<(SegmentId, GroupPageKind)> {
        self.require(PageKind::Grouped)?;
        let mut reader = Reader::new(self.bytes().get(HEADER..).ok_or(Error::InvalidState)?);
        if reader.take(4)? != b"G9PG" || reader.u16()? != 1 {
            return Err(Error::InvalidState);
        }
        let kind = match reader.u8()? {
            1 => GroupPageKind::Members,
            2 => GroupPageKind::Liveness,
            3 => GroupPageKind::Posting,
            4 => GroupPageKind::Leaf,
            5 => GroupPageKind::Branch,
            _ => return Err(Error::InvalidState),
        };
        if reader.u8()? != 0 {
            return Err(Error::InvalidState);
        }
        Ok((
            SegmentId::new(reader.u64()?).map_err(|_| Error::InvalidState)?,
            kind,
        ))
    }

    /// creates one bounded record fragment; allocation links are independent of keys.
    pub fn group_record(
        block: u32,
        id: SegmentId,
        kind: GroupPageKind,
        key: [u64; 2],
        total: u32,
        offset: u32,
        bytes: &[u8],
    ) -> Result<Self> {
        let mut page = Self::new_grouped(block, id, kind)?;
        if bytes.len() > GROUP_DATA_BYTES {
            return Err(Error::Limit("group fragment"));
        }
        let mut writer = Writer::new(&mut page.bytes[GROUP_HEADER..]);
        writer.u64(key[0])?;
        writer.u64(key[1])?;
        writer.u32(total)?;
        writer.u32(offset)?;
        writer.put(bytes)?;
        page.len = DATA_HEADER + bytes.len();
        page.group_data()?;
        Ok(page)
    }

    pub fn group_data(&self) -> Result<GroupData<'_>> {
        let (id, kind) = self.group_identity()?;
        if !matches!(
            kind,
            GroupPageKind::Members | GroupPageKind::Liveness | GroupPageKind::Posting
        ) {
            return Err(Error::InvalidState);
        }
        let mut reader = Reader::new(&self.bytes()[GROUP_HEADER..]);
        let key = [reader.u64()?, reader.u64()?];
        let total = reader.u32()?;
        let offset = reader.u32()?;
        let bytes = self.bytes().get(DATA_HEADER..).ok_or(Error::InvalidState)?;
        let maximum = if kind == GroupPageKind::Members {
            72 + 256 * 512 * 16
        } else {
            72 + 256 * 68
        };
        if !valid_key(key)
            || (kind == GroupPageKind::Posting) != (key[0] != 0)
            || total < 72
            || total as usize > maximum
            || offset >= total
            || !(offset as usize).is_multiple_of(GROUP_DATA_BYTES)
            || bytes.len() != ((total - offset) as usize).min(GROUP_DATA_BYTES)
        {
            return Err(Error::InvalidState);
        }
        Ok(GroupData {
            id,
            kind,
            key,
            total,
            offset,
            bytes,
        })
    }

    pub fn group_node(
        block: u32,
        id: SegmentId,
        level: u8,
        entries: &[CatalogEntry],
    ) -> Result<Self> {
        if level > MAX_CATALOG_LEVEL || entries.is_empty() || entries.len() > CATALOG_ENTRIES {
            return Err(Error::Limit("group catalog node"));
        }
        let kind = if level == 0 {
            GroupPageKind::Leaf
        } else {
            GroupPageKind::Branch
        };
        let mut page = Self::new_grouped(block, id, kind)?;
        let mut writer = Writer::new(&mut page.bytes[GROUP_HEADER..]);
        writer.u8(level)?;
        writer.u8(0)?;
        writer.u16(entries.len() as u16)?;
        writer.u32(0)?;
        for entry in entries {
            writer.u64(entry.key[0])?;
            writer.u64(entry.key[1])?;
            writer.put(&entry.value)?;
        }
        page.len = NODE_HEADER + entries.len() * ENTRY_BYTES;
        page.validate_group_node()?;
        Ok(page)
    }

    pub fn group_node_info(&self) -> Result<(u8, u16)> {
        let (_, kind) = self.group_identity()?;
        let level = *self.bytes().get(GROUP_HEADER).ok_or(Error::InvalidState)?;
        let count = self.u16(GROUP_HEADER + 2)?;
        if level > MAX_CATALOG_LEVEL
            || count == 0
            || usize::from(count) > CATALOG_ENTRIES
            || self.bytes[GROUP_HEADER + 1] != 0
            || self.u32(GROUP_HEADER + 4)? != 0
            || self.len != NODE_HEADER + usize::from(count) * ENTRY_BYTES
            || kind
                != if level == 0 {
                    GroupPageKind::Leaf
                } else {
                    GroupPageKind::Branch
                }
        {
            return Err(Error::InvalidState);
        }
        Ok((level, count))
    }

    pub(crate) fn group_node_view(&self) -> Result<GroupNode<'_>> {
        let (level, count) = self.group_node_info()?;
        Ok(GroupNode {
            bytes: &self.bytes[NODE_HEADER..self.len],
            level,
            count,
        })
    }

    pub fn group_entry(&self, index: u16) -> Result<CatalogEntry> {
        self.group_node_view()?.entry(index)
    }

    fn validate_group_node(&self) -> Result<()> {
        let node = self.group_node_view()?;
        let mut previous = None;
        for index in 0..node.count {
            let key = node.key(index)?;
            if !valid_key(key) || previous.is_some_and(|previous| previous >= key) {
                return Err(Error::InvalidState);
            }
            if node.level != 0 {
                let entry = node.entry(index)?;
                let child = u32::from_le_bytes(
                    entry.value[..4]
                        .try_into()
                        .map_err(|_| Error::InvalidState)?,
                );
                if !super::block_valid(child) || child == self.block || entry.value[4..] != [0; 60]
                {
                    return Err(Error::InvalidState);
                }
            }
            previous = Some(key);
        }
        Ok(())
    }

    pub(super) fn validate_grouped(&self, _layout: HeapLayout) -> Result<()> {
        match self.group_identity()?.1 {
            GroupPageKind::Leaf | GroupPageKind::Branch => self.validate_group_node(),
            _ => self.group_data().map(|_| ()),
        }
    }
}
