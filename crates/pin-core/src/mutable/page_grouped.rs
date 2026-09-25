//! checked physical envelopes for grouped records and immutable catalog nodes.
//! legacy metadata remains readable; grouped state occupies an optional tail.

use super::{BUCKETS, HEADER, META_HEADER, NO_BLOCK, OwnerRef, Page, PageKind, RewritePhase};
use crate::codec::bytes::{Reader, Writer};
use crate::error::{Error, Result};
use crate::identity::{HeapLayout, SegmentId};

pub(super) const META_LEGACY_BYTES: usize = META_HEADER + BUCKETS * 8;
pub(super) const META_GROUPED_BYTES: usize = META_LEGACY_BYTES + 72;
pub(super) const GROUP_DELTA_SEGMENTS: usize = 16;
pub(super) const GROUP_RETIRED_SEGMENTS: usize = GROUP_DELTA_SEGMENTS + 1;
const SNAPSHOT_BYTES: usize = 48;
const DELTA_BYTES: usize = SNAPSHOT_BYTES + 24;
const JOURNAL_BYTES: usize = 24;
const RETIRED_BYTES: usize = 16;
pub(super) const META_GROUPED_V3_BYTES: usize = META_LEGACY_BYTES
    + 8
    + SNAPSHOT_BYTES
    + JOURNAL_BYTES
    + GROUP_DELTA_SEGMENTS * DELTA_BYTES
    + GROUP_RETIRED_SEGMENTS * RETIRED_BYTES;
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
    /// a separate catalog in the same allocation journal; none is the v1 format.
    pub frontier_root: Option<u32>,
    /// cleared durably before any canonical posting-chain rewrite.
    pub frontier_valid: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupJournal {
    pub id: SegmentId,
    pub head: u32,
    pub tail: u32,
    pub phase: RewritePhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupDelta {
    pub snapshot: GroupSnapshot,
    pub before: Option<OwnerRef>,
    pub level: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupRetired {
    pub id: SegmentId,
    pub head: u32,
    pub tail: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupState {
    pub active: Option<GroupSnapshot>,
    pub journal: Option<GroupJournal>,
    pub deltas: [Option<GroupDelta>; GROUP_DELTA_SEGMENTS],
    pub retired: [Option<GroupRetired>; GROUP_RETIRED_SEGMENTS],
}

impl Default for GroupState {
    fn default() -> Self {
        Self {
            active: None,
            journal: None,
            deltas: [None; GROUP_DELTA_SEGMENTS],
            retired: [None; GROUP_RETIRED_SEGMENTS],
        }
    }
}

impl GroupState {
    pub fn latest(self) -> Option<GroupSnapshot> {
        self.deltas
            .iter()
            .rev()
            .flatten()
            .next()
            .map(|delta| delta.snapshot)
            .or(self.active)
    }

    pub fn delta_count(self) -> usize {
        self.deltas.iter().flatten().count()
    }
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
    fn validate_snapshot(snapshot: GroupSnapshot, allow_frontier: bool) -> Result<()> {
        if !pair(snapshot.head, snapshot.tail)
            || (snapshot.head == NO_BLOCK)
                != (snapshot.root == NO_BLOCK
                    && snapshot.frontier_root.unwrap_or(NO_BLOCK) == NO_BLOCK)
            || (!allow_frontier && snapshot.frontier_root.is_some())
            || (snapshot.frontier_root.is_none() && snapshot.root != snapshot.tail)
            || (snapshot.root != NO_BLOCK && !super::block_valid(snapshot.root))
            || snapshot.frontier_root.is_some_and(|root| {
                root != NO_BLOCK && (!super::block_valid(root) || root == snapshot.root)
            })
            || (snapshot.frontier_valid && snapshot.frontier_root.is_none())
            || snapshot
                .after
                .is_some_and(|owner| owner.incarnation.get() >= snapshot.id.get())
        {
            return Err(Error::InvalidState);
        }
        Ok(())
    }

    fn validate(self) -> Result<()> {
        if let Some(active) = self.active {
            Self::validate_snapshot(active, true)?;
        }
        let mut previous_after = self.active.and_then(|active| active.after);
        let mut previous_id = self.active.map(|active| active.id);
        let mut previous_level = None;
        let mut gap = false;
        for delta in self.deltas {
            let Some(delta) = delta else {
                gap = true;
                continue;
            };
            if gap || self.active.is_none() {
                return Err(Error::InvalidState);
            }
            Self::validate_snapshot(delta.snapshot, false)?;
            if delta.snapshot.frontier_valid
                || usize::from(delta.level) >= GROUP_DELTA_SEGMENTS
                || previous_level.is_some_and(|level| level <= delta.level)
                || delta.before != previous_after
                || delta.snapshot.after.is_none()
                || delta.snapshot.after == delta.before
                || delta.before.is_some_and(|before| {
                    delta
                        .snapshot
                        .after
                        .is_some_and(|after| before.incarnation >= after.incarnation)
                })
                || previous_id.is_some_and(|id| id >= delta.snapshot.id)
            {
                return Err(Error::InvalidState);
            }
            previous_after = delta.snapshot.after;
            previous_id = Some(delta.snapshot.id);
            previous_level = Some(delta.level);
        }
        if let Some(journal) = self.journal {
            if !pair(journal.head, journal.tail) {
                return Err(Error::InvalidState);
            }
            match journal.phase {
                RewritePhase::Building => {
                    if previous_id.is_some_and(|id| journal.id <= id) {
                        return Err(Error::InvalidState);
                    }
                }
                RewritePhase::Retiring => {
                    if self.delta_count() != 0
                        || self.retired.iter().any(Option::is_some)
                        || journal.head == NO_BLOCK
                        || self.active.is_none()
                        || self.active.is_some_and(|active| journal.id >= active.id)
                    {
                        return Err(Error::InvalidState);
                    }
                }
            }
        }
        let mut retired_gap = false;
        for retired in self.retired {
            let Some(retired) = retired else {
                retired_gap = true;
                continue;
            };
            if retired_gap || retired.head == NO_BLOCK || !pair(retired.head, retired.tail) {
                return Err(Error::InvalidState);
            }
            if self.active.is_some_and(|active| active.id == retired.id)
                || self
                    .deltas
                    .iter()
                    .flatten()
                    .any(|delta| delta.snapshot.id == retired.id)
                || self.journal.is_some_and(|journal| journal.id == retired.id)
            {
                return Err(Error::InvalidState);
            }
        }
        Ok(())
    }
}

impl Page {
    /// tests a captured allocation fence, not transaction visibility.
    pub(crate) fn grouped_has_delta(&self, snapshot: GroupSnapshot) -> Result<bool> {
        Ok(self.grouped_delta_span(snapshot)? != 0)
    }

    /// returns reserved incarnations after one captured grouped allocation fence.
    pub(crate) fn grouped_delta_span(&self, snapshot: GroupSnapshot) -> Result<u64> {
        self.require(PageKind::Meta)?;
        let next = self.u64(32)?;
        if next <= snapshot.id.get() {
            return Err(Error::InvalidState);
        }
        Ok(next - snapshot.id.get() - 1)
    }

    fn read_snapshot(reader: &mut Reader<'_>) -> Result<Option<GroupSnapshot>> {
        let id = reader.u64()?;
        let head = reader.u32()?;
        let tail = reader.u32()?;
        let root = reader.u32()?;
        let frontier = reader.u32()?;
        let flags = reader.u32()?;
        if reader.u32()? != 0 || flags > 1 {
            return Err(Error::InvalidState);
        }
        let owner = reader.take(16)?;
        let after = if owner == [0; 16] {
            None
        } else {
            Some(OwnerRef::read(&mut Reader::new(owner))?)
        };
        if id == 0 {
            if head != NO_BLOCK
                || tail != NO_BLOCK
                || root != NO_BLOCK
                || frontier != 0
                || flags != 0
                || after.is_some()
            {
                return Err(Error::InvalidState);
            }
            return Ok(None);
        }
        Ok(Some(GroupSnapshot {
            id: SegmentId::new(id).map_err(|_| Error::InvalidState)?,
            head,
            tail,
            root,
            after,
            frontier_root: (frontier != 0).then_some(frontier),
            frontier_valid: flags == 1,
        }))
    }

    fn write_snapshot(writer: &mut Writer<'_>, snapshot: Option<GroupSnapshot>) -> Result<()> {
        writer.u64(snapshot.map_or(0, |snapshot| snapshot.id.get()))?;
        writer.u32(snapshot.map_or(NO_BLOCK, |snapshot| snapshot.head))?;
        writer.u32(snapshot.map_or(NO_BLOCK, |snapshot| snapshot.tail))?;
        writer.u32(snapshot.map_or(NO_BLOCK, |snapshot| snapshot.root))?;
        writer.u32(snapshot.and_then(|snapshot| snapshot.frontier_root).unwrap_or(0))?;
        writer.u32(u32::from(snapshot.is_some_and(|snapshot| snapshot.frontier_valid)))?;
        writer.u32(0)?;
        if let Some(owner) = snapshot.and_then(|snapshot| snapshot.after) {
            owner.write(writer)?;
        } else {
            writer.put(&[0; 16])?;
        }
        Ok(())
    }

    fn read_old_grouped_state(&self) -> Result<GroupState> {
        let mut reader = Reader::new(&self.bytes[META_LEGACY_BYTES..self.len]);
        if reader.take(4)? != b"PG09" {
            return Err(Error::InvalidState);
        }
        let version = reader.u16()?;
        let flags = reader.u16()?;
        if !matches!((version, flags), (1, 0) | (2, 0..=1)) {
            return Err(Error::InvalidState);
        }
        let id = reader.u64()?;
        let head = reader.u32()?;
        let tail = reader.u32()?;
        let root = reader.u32()?;
        let frontier = reader.u32()?;
        if version == 1 && frontier != 0 {
            return Err(Error::InvalidState);
        }
        let frontier_root = (version == 2).then_some(frontier);
        let owner = reader.take(16)?;
        let after = if owner == [0; 16] {
            None
        } else {
            Some(OwnerRef::read(&mut Reader::new(owner))?)
        };
        let active = if id == 0 {
            if head != NO_BLOCK
                || tail != NO_BLOCK
                || root != NO_BLOCK
                || after.is_some()
                || version != 1
            {
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
                frontier_root,
                frontier_valid: flags == 1,
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
        Ok(GroupState {
            active,
            journal,
            ..GroupState::default()
        })
    }

    /// reads legacy, pg09, or bounded-delta pg10 metadata.
    pub fn grouped_state(&self) -> Result<GroupState> {
        self.require(PageKind::Meta)?;
        if self.len == META_LEGACY_BYTES {
            return Ok(GroupState::default());
        }
        let state = if self.len == META_GROUPED_BYTES {
            self.read_old_grouped_state()?
        } else if self.len == META_GROUPED_V3_BYTES {
            let mut reader = Reader::new(&self.bytes[META_LEGACY_BYTES..self.len]);
            if reader.take(4)? != b"PG10" || reader.u16()? != 1 || reader.u16()? != 0 {
                return Err(Error::InvalidState);
            }
            let active = Self::read_snapshot(&mut reader)?;
            let id = reader.u64()?;
            let head = reader.u32()?;
            let tail = reader.u32()?;
            let phase = reader.u32()?;
            if reader.u32()? != 0 {
                return Err(Error::InvalidState);
            }
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
            let mut deltas = [None; GROUP_DELTA_SEGMENTS];
            for slot in &mut deltas {
                let snapshot = Self::read_snapshot(&mut reader)?;
                let before = reader.take(16)?;
                let before = if before == [0; 16] {
                    None
                } else {
                    Some(OwnerRef::read(&mut Reader::new(before))?)
                };
                let level = reader.u8()?;
                if reader.take(7)? != [0; 7] {
                    return Err(Error::InvalidState);
                }
                *slot = snapshot.map(|snapshot| GroupDelta { snapshot, before, level });
                if snapshot.is_none() && (before.is_some() || level != 0) {
                    return Err(Error::InvalidState);
                }
            }
            let mut retired = [None; GROUP_RETIRED_SEGMENTS];
            for slot in &mut retired {
                let id = reader.u64()?;
                let head = reader.u32()?;
                let tail = reader.u32()?;
                *slot = if id == 0 {
                    if head != NO_BLOCK || tail != NO_BLOCK {
                        return Err(Error::InvalidState);
                    }
                    None
                } else {
                    Some(GroupRetired {
                        id: SegmentId::new(id).map_err(|_| Error::InvalidState)?,
                        head,
                        tail,
                    })
                };
            }
            reader.finish()?;
            GroupState {
                active,
                journal,
                deltas,
                retired,
            }
        } else {
            return Err(Error::InvalidState);
        };
        state.validate()?;
        let next = self.u64(32)?;
        if state.active.is_some_and(|active| active.id.get() >= next)
            || state
                .deltas
                .iter()
                .flatten()
                .any(|delta| delta.snapshot.id.get() >= next)
            || state
                .journal
                .is_some_and(|journal| journal.id.get() >= next)
            || state
                .retired
                .iter()
                .flatten()
                .any(|retired| retired.id.get() >= next)
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
                .deltas
                .iter()
                .flatten()
                .any(|delta| delta.snapshot.id.get() >= next)
            || state
                .journal
                .is_some_and(|journal| journal.id.get() >= next)
            || state
                .retired
                .iter()
                .flatten()
                .any(|retired| retired.id.get() >= next)
        {
            return Err(Error::InvalidState);
        }
        let extended = state.delta_count() != 0 || state.retired.iter().any(Option::is_some);
        if !extended {
            let mut bytes = [0u8; 72];
            let mut writer = Writer::new(&mut bytes);
            let frontier = state.active.and_then(|active| active.frontier_root);
            writer.put(b"PG09")?;
            writer.u16(if frontier.is_some() { 2 } else { 1 })?;
            writer.u16(u16::from(
                state.active.is_some_and(|active| active.frontier_valid),
            ))?;
            writer.u64(state.active.map_or(0, |active| active.id.get()))?;
            writer.u32(state.active.map_or(NO_BLOCK, |active| active.head))?;
            writer.u32(state.active.map_or(NO_BLOCK, |active| active.tail))?;
            writer.u32(state.active.map_or(NO_BLOCK, |active| active.root))?;
            writer.u32(frontier.unwrap_or(0))?;
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
            return Ok(());
        }
        let mut bytes = [0u8; META_GROUPED_V3_BYTES - META_LEGACY_BYTES];
        let mut writer = Writer::new(&mut bytes);
        writer.put(b"PG10")?;
        writer.u16(1)?;
        writer.u16(0)?;
        Self::write_snapshot(&mut writer, state.active)?;
        writer.u64(state.journal.map_or(0, |journal| journal.id.get()))?;
        writer.u32(state.journal.map_or(NO_BLOCK, |journal| journal.head))?;
        writer.u32(state.journal.map_or(NO_BLOCK, |journal| journal.tail))?;
        writer.u32(state.journal.map_or(0, |journal| journal.phase as u32))?;
        writer.u32(0)?;
        for delta in state.deltas {
            Self::write_snapshot(&mut writer, delta.map(|delta| delta.snapshot))?;
            if let Some(before) = delta.and_then(|delta| delta.before) {
                before.write(&mut writer)?;
            } else {
                writer.put(&[0; 16])?;
            }
            writer.u8(delta.map_or(0, |delta| delta.level))?;
            writer.put(&[0; 7])?;
        }
        for retired in state.retired {
            writer.u64(retired.map_or(0, |retired| retired.id.get()))?;
            writer.u32(retired.map_or(NO_BLOCK, |retired| retired.head))?;
            writer.u32(retired.map_or(NO_BLOCK, |retired| retired.tail))?;
        }
        writer.finish()?;
        self.bytes[META_LEGACY_BYTES..META_GROUPED_V3_BYTES].copy_from_slice(&bytes);
        self.len = META_GROUPED_V3_BYTES;
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
