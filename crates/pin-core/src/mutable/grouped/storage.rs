//! journal-owned physical records and a bounded immutable catalog.
//! publication and retirement use the existing atomic page-store contract.

use super::super::page::{
    CATALOG_ENTRIES, CatalogEntry, GROUP_DATA_BYTES, GROUP_DELTA_SEGMENTS, GROUP_RETIRED_SEGMENTS,
    GroupDelta, GroupJournal, GroupPageKind, GroupRetired, GroupSnapshot, GroupState,
    MAX_CATALOG_LEVEL, NO_BLOCK, OwnerRef, Page, PageKind, RewritePhase,
};
use super::super::{PageStore, Stage, allocate, load, posting_next};
use crate::error::{Error, Result};
use crate::grouped::{Bitmap, BitmapKind, GroupKey, PageOffsets, encode_bitmap};
use crate::identity::{Generation, HeapLayout, SegmentId};
use pin_kernels::grouped::PageMask;

pub(super) const BITMAP_BYTES: usize = 72 + 256 * 68;
pub(super) const MEMBER_BYTES: usize = 72 + 256 * 512 * 16;
const LEVELS: usize = MAX_CATALOG_LEVEL as usize + 1;
const INLINE_POSTINGS: usize = 18;
pub(super) const CATALOG_MEMORY: usize =
    LEVELS * CATALOG_ENTRIES * core::mem::size_of::<CatalogEntry>();

#[derive(Clone, Copy, Debug)]
pub(super) struct Value {
    pub len: u32,
    pub blocks: [u32; 3],
    pub pages: PageMask,
    pub members: u32,
    pub member_bytes: u32,
    pub count: u32,
    pub(super) inline: Option<[u8; INLINE_POSTINGS * 3]>,
}

impl Value {
    fn inline_bytes(self) -> [u8; INLINE_POSTINGS * 3] {
        if let Some(bytes) = self.inline {
            return bytes;
        }
        let mut bytes = [0; INLINE_POSTINGS * 3];
        for (index, block) in self.blocks.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&block.to_le_bytes());
        }
        bytes[12..16].copy_from_slice(&self.members.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.member_bytes.to_le_bytes());
        bytes
    }

    fn inline_pages(self, max_offset: u16) -> Result<([PageOffsets; INLINE_POSTINGS], usize)> {
        let count = usize::try_from(self.count).map_err(|_| Error::InvalidState)?;
        if self.len != 0 || count == 0 || count > self.inline.map_or(6, |_| INLINE_POSTINGS) {
            return Err(Error::InvalidState);
        }
        let bytes = self.inline_bytes();
        if bytes[count * 3..].iter().any(|&byte| byte != 0) {
            return Err(Error::InvalidState);
        }
        let mut pages: [PageOffsets; INLINE_POSTINGS] = core::array::from_fn(|_| PageOffsets {
            page: 0,
            offsets: [0; 8],
        });
        let mut used = 0usize;
        let mut previous = None;
        let mut mask = [0u64; 4];
        for chunk in bytes[..count * 3].as_chunks::<3>().0 {
            let coordinate =
                u32::from(chunk[0]) | (u32::from(chunk[1]) << 8) | (u32::from(chunk[2]) << 16);
            let page = (coordinate >> 9) as u8;
            let offset = (coordinate & 511) as u16 + 1;
            if offset > max_offset || previous.is_some_and(|old| old >= coordinate) {
                return Err(Error::InvalidState);
            }
            previous = Some(coordinate);
            mask[usize::from(page) / 64] |= 1 << (page % 64);
            if used == 0 || pages[used - 1].page != page {
                pages[used].page = page;
                used += 1;
            }
            let bit = usize::from(offset - 1);
            pages[used - 1].offsets[bit / 64] |= 1 << (bit % 64);
        }
        if self.inline.is_none() && mask != self.pages {
            return Err(Error::InvalidState);
        }
        Ok((pages, used))
    }

    pub fn entry(self, key: [u64; 2]) -> CatalogEntry {
        let mut value = [0; 64];
        value[..4].copy_from_slice(&self.len.to_le_bytes());
        if let Some(inline) = self.inline {
            value[4..58].copy_from_slice(&inline);
            value[60..64].copy_from_slice(&self.count.to_le_bytes());
            return CatalogEntry { key, value };
        }
        for (index, block) in self.blocks.iter().enumerate() {
            value[4 + index * 4..8 + index * 4].copy_from_slice(&block.to_le_bytes());
        }
        for (index, word) in self.pages.iter().enumerate() {
            value[16 + index * 8..24 + index * 8].copy_from_slice(&word.to_le_bytes());
        }
        value[48..52].copy_from_slice(&self.members.to_le_bytes());
        value[52..56].copy_from_slice(&self.member_bytes.to_le_bytes());
        value[56..60].copy_from_slice(&self.count.to_le_bytes());
        CatalogEntry { key, value }
    }

    pub fn read(entry: CatalogEntry) -> Result<Self> {
        use crate::codec::bytes::Reader;
        if entry.value[..4] == [0; 4] && entry.value[60..64] != [0; 4] {
            if entry.key[0] == 0 || entry.value[58..60] != [0; 2] {
                return Err(Error::InvalidState);
            }
            let mut inline = [0; INLINE_POSTINGS * 3];
            inline.copy_from_slice(&entry.value[4..58]);
            let mut value = Self {
                len: 0,
                blocks: [NO_BLOCK; 3],
                pages: [0; 4],
                members: NO_BLOCK,
                member_bytes: 0,
                count: u32::from_le_bytes([
                    entry.value[60],
                    entry.value[61],
                    entry.value[62],
                    entry.value[63],
                ]),
                inline: Some(inline),
            };
            let (pages, used) = value.inline_pages(512)?;
            for page in &pages[..used] {
                value.pages[usize::from(page.page) / 64] |= 1 << (page.page % 64);
            }
            return Ok(value);
        }
        let mut reader = Reader::new(&entry.value);
        let len = reader.u32()?;
        let blocks = [reader.u32()?, reader.u32()?, reader.u32()?];
        let pages = [reader.u64()?, reader.u64()?, reader.u64()?, reader.u64()?];
        let members = reader.u32()?;
        let member_bytes = reader.u32()?;
        let count = reader.u32()?;
        if reader.u32()? != 0 {
            return Err(Error::InvalidState);
        }
        if len == 0 {
            let value = Self {
                len,
                blocks,
                pages,
                members,
                member_bytes,
                count,
                inline: None,
            };
            if entry.key[0] == 0 {
                return Err(Error::InvalidState);
            }
            value.inline_pages(512)?;
            return Ok(value);
        }
        if !(72..=BITMAP_BYTES as u32).contains(&len) {
            return Err(Error::InvalidState);
        }
        let used = (len as usize).div_ceil(GROUP_DATA_BYTES);
        for (index, &block) in blocks.iter().enumerate() {
            if index < used {
                if block == 0 || block == NO_BLOCK || blocks[..index].contains(&block) {
                    return Err(Error::InvalidState);
                }
            } else if block != NO_BLOCK {
                return Err(Error::InvalidState);
            }
        }
        if entry.key[0] == 0 {
            if members == 0
                || members == NO_BLOCK
                || count == 0
                || count > 256 * 512
                || member_bytes != 72 + count * 16
                || blocks.contains(&members)
            {
                return Err(Error::InvalidState);
            }
        } else if members != NO_BLOCK || member_bytes != 0 || count != 0 {
            return Err(Error::InvalidState);
        }
        if pages == [0; 4] {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            len,
            blocks,
            pages,
            members,
            member_bytes,
            count,
            inline: None,
        })
    }
}

/// embeds sparse postings in the catalog leaf, avoiding one WAL page per term.
pub(super) fn inline_postings(
    key: [u64; 2],
    pages: &[PageOffsets],
    layout: HeapLayout,
) -> Result<Option<CatalogEntry>> {
    if key[0] == 0 || pages.is_empty() {
        return Err(Error::InvalidState);
    }
    let mut bytes = [0u8; INLINE_POSTINGS * 3];
    let mut mask = [0u64; 4];
    let mut count = 0usize;
    let mut previous_page = None;
    for page in pages {
        if previous_page.is_some_and(|old| old >= page.page) {
            return Err(Error::InvalidState);
        }
        previous_page = Some(page.page);
        mask[usize::from(page.page) / 64] |= 1 << (page.page % 64);
        for (word_index, &word) in page.offsets.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let bit = remaining.trailing_zeros() as usize;
                let offset = word_index * 64 + bit + 1;
                if offset > usize::from(layout.max_offset()) {
                    return Err(Error::InvalidState);
                }
                if count == INLINE_POSTINGS {
                    return Ok(None);
                }
                let coordinate = (u32::from(page.page) << 9) | (offset as u32 - 1);
                bytes[count * 3..count * 3 + 3].copy_from_slice(&coordinate.to_le_bytes()[..3]);
                count += 1;
                remaining &= remaining - 1;
            }
        }
    }
    if count == 0 {
        return Err(Error::InvalidState);
    }
    let value = Value {
        len: 0,
        blocks: [NO_BLOCK; 3],
        pages: mask,
        members: NO_BLOCK,
        member_bytes: 0,
        count: count as u32,
        inline: Some(bytes),
    };
    value.inline_pages(layout.max_offset())?;
    Ok(Some(value.entry(key)))
}

pub(super) fn key(snapshot: GroupSnapshot, base: u32, layout: HeapLayout) -> Result<GroupKey> {
    // the page store already selects a single physical relation generation.
    GroupKey::new(
        Generation::new(1).map_err(|_| Error::InvalidState)?,
        snapshot.id,
        base,
        layout,
    )
}

pub(super) fn begin<S: PageStore>(store: &mut S, after: Option<OwnerRef>) -> Result<GroupSnapshot> {
    recover(store)?;
    let mut meta = load(store, 0, PageKind::Meta)?;
    let mut state = meta.grouped_state()?;
    let id = SegmentId::new(meta.reserve_incarnation()?.get()).map_err(|_| Error::InvalidState)?;
    state.journal = Some(GroupJournal {
        id,
        head: NO_BLOCK,
        tail: NO_BLOCK,
        phase: RewritePhase::Building,
    });
    meta.set_grouped_state(state)?;
    store.commit(&[&meta])?;
    store.event(Stage::GroupReserved)?;
    Ok(GroupSnapshot {
        id,
        head: NO_BLOCK,
        tail: NO_BLOCK,
        root: NO_BLOCK,
        after,
        frontier_root: None,
        frontier_valid: false,
    })
}

fn append<S: PageStore>(
    store: &mut S,
    snapshot: &mut GroupSnapshot,
    make: impl FnOnce(u32) -> Result<Page>,
) -> Result<u32> {
    let mut meta = load(store, 0, PageKind::Meta)?;
    let mut state = meta.grouped_state()?;
    let journal = state.journal.ok_or(Error::InvalidState)?;
    if journal.id != snapshot.id
        || journal.phase != RewritePhase::Building
        || journal.head != snapshot.head
        || journal.tail != snapshot.tail
    {
        return Err(Error::InvalidState);
    }
    let free = meta.free_head()?;
    let block = if free == NO_BLOCK {
        allocate(store)?
    } else {
        let page = load(store, free, PageKind::Free)?;
        meta.set_free_head(page.next()?)?;
        free
    };
    let page = make(block)?;
    if page.group_identity()?.0 != snapshot.id || page.next()? != NO_BLOCK {
        return Err(Error::InvalidState);
    }
    let head = if journal.head == NO_BLOCK {
        block
    } else {
        journal.head
    };
    state.journal = Some(GroupJournal {
        head,
        tail: block,
        ..journal
    });
    meta.set_grouped_state(state)?;
    if journal.tail == NO_BLOCK {
        store.commit(&[&meta, &page])?;
    } else {
        let mut previous = load(store, journal.tail, PageKind::Grouped)?;
        if previous.group_identity()?.0 != snapshot.id || previous.next()? != NO_BLOCK {
            return Err(Error::InvalidState);
        }
        previous.set_next(block)?;
        store.commit(&[&meta, &previous, &page])?;
    }
    snapshot.head = head;
    snapshot.tail = block;
    store.event(Stage::GroupStored)?;
    Ok(block)
}

pub(super) fn write_record<S: PageStore>(
    store: &mut S,
    snapshot: &mut GroupSnapshot,
    kind: GroupPageKind,
    key: [u64; 2],
    bytes: &[u8],
) -> Result<(u32, [u32; 3])> {
    let total = u32::try_from(bytes.len()).map_err(|_| Error::Limit("group record"))?;
    let mut head = NO_BLOCK;
    let mut blocks = [NO_BLOCK; 3];
    let id = snapshot.id;
    for (index, chunk) in bytes.chunks(GROUP_DATA_BYTES).enumerate() {
        let block = append(store, snapshot, |block| {
            Page::group_record(
                block,
                id,
                kind,
                key,
                total,
                (index * GROUP_DATA_BYTES) as u32,
                chunk,
            )
        })?;
        if index == 0 {
            head = block;
        }
        if let Some(slot) = blocks.get_mut(index) {
            *slot = block;
        }
    }
    if head == NO_BLOCK {
        return Err(Error::InvalidState);
    }
    Ok((head, blocks))
}

pub(super) fn read_record<S: PageStore>(
    store: &mut S,
    snapshot: GroupSnapshot,
    kind: GroupPageKind,
    key: [u64; 2],
    head: u32,
    output: &mut [u8],
    expected_blocks: Option<[u32; 3]>,
) -> Result<()> {
    let mut block = head;
    let total = output.len();
    for (index, chunk) in output.chunks_mut(GROUP_DATA_BYTES).enumerate() {
        if expected_blocks.is_some_and(|blocks| blocks.get(index) != Some(&block)) {
            return Err(Error::InvalidState);
        }
        let page = load(store, block, PageKind::Grouped)?;
        let data = page.group_data()?;
        if data.id != snapshot.id
            || data.kind != kind
            || data.key != key
            || data.total as usize != total
            || data.offset as usize != index * GROUP_DATA_BYTES
            || data.bytes.len() != chunk.len()
        {
            return Err(Error::InvalidState);
        }
        chunk.copy_from_slice(data.bytes);
        block = page.next()?;
    }
    Ok(())
}

pub(super) fn read_bitmap<S: PageStore>(
    store: &mut S,
    snapshot: GroupSnapshot,
    entry: CatalogEntry,
    output: &mut [u8],
) -> Result<Value> {
    read_bitmap_view(store, snapshot, entry, output).map(|(value, _)| value)
}

pub(super) fn read_bitmap_view<'a, S: PageStore>(
    store: &mut S,
    snapshot: GroupSnapshot,
    entry: CatalogEntry,
    output: &'a mut [u8],
) -> Result<(Value, Bitmap<'a>)> {
    let value = Value::read(entry)?;
    if value.len == 0 {
        let (pages, count) = value.inline_pages(store.layout().max_offset())?;
        let len = encode_bitmap(
            key(snapshot, entry.key[1] as u32, store.layout())?,
            BitmapKind::Posting,
            &pages[..count],
            output,
        )?;
        let view = Bitmap::open(&output[..len])?;
        return Ok((value, view));
    }
    let output = output
        .get_mut(..value.len as usize)
        .ok_or(Error::Limit("group bitmap scratch"))?;
    let kind = if entry.key[0] == 0 {
        GroupPageKind::Liveness
    } else {
        GroupPageKind::Posting
    };
    read_record(
        store,
        snapshot,
        kind,
        entry.key,
        value.blocks[0],
        output,
        Some(value.blocks),
    )?;
    let view = Bitmap::open(output)?;
    let expected = if entry.key[0] == 0 {
        BitmapKind::Liveness
    } else {
        BitmapKind::Posting
    };
    if view.key() != key(snapshot, entry.key[1] as u32, store.layout())?
        || view.kind() != expected
        || *view.pages() != value.pages
    {
        return Err(Error::InvalidState);
    }
    Ok((value, view))
}

fn verify_build<S: PageStore>(
    store: &mut S,
    state: GroupState,
    snapshot: GroupSnapshot,
) -> Result<u32> {
    if snapshot.frontier_root.is_none() && snapshot.root != snapshot.tail {
        return Err(Error::InvalidState);
    }
    let written = inspect(store, snapshot.id, snapshot.head, snapshot.tail)?;
    for root in [Some(snapshot.root), snapshot.frontier_root]
        .into_iter()
        .flatten()
    {
        if root != NO_BLOCK {
            let page = load(store, root, PageKind::Grouped)?;
            if page.group_identity()?.0 != snapshot.id {
                return Err(Error::InvalidState);
            }
            page.group_node_info()?;
        }
    }
    let journal = state.journal.ok_or(Error::InvalidState)?;
    if journal.id != snapshot.id
        || journal.phase != RewritePhase::Building
        || journal.head != snapshot.head
        || journal.tail != snapshot.tail
    {
        return Err(Error::InvalidState);
    }
    Ok(written)
}

fn retired(snapshot: GroupSnapshot) -> Option<GroupRetired> {
    (snapshot.head != NO_BLOCK).then_some(GroupRetired {
        id: snapshot.id,
        head: snapshot.head,
        tail: snapshot.tail,
    })
}

pub(super) fn publish<S: PageStore>(store: &mut S, snapshot: GroupSnapshot) -> Result<(u32, u32)> {
    let mut meta = load(store, 0, PageKind::Meta)?;
    let state = meta.grouped_state()?;
    let written = verify_build(store, state, snapshot)?;
    if state.retired.iter().any(Option::is_some) {
        return Err(Error::InvalidState);
    }
    let mut retiring = [None; GROUP_RETIRED_SEGMENTS];
    let mut used = 0usize;
    for old in state.active.into_iter().chain(
        state
            .deltas
            .into_iter()
            .flatten()
            .map(|delta| delta.snapshot),
    ) {
        if let Some(old) = retired(old) {
            let slot = retiring
                .get_mut(used)
                .ok_or(Error::Limit("group retire queue"))?;
            *slot = Some(old);
            used += 1;
        }
    }
    meta.set_grouped_state(GroupState {
        active: Some(snapshot),
        journal: None,
        deltas: [None; GROUP_DELTA_SEGMENTS],
        retired: retiring,
    })?;
    store.commit(&[&meta])?;
    store.event(Stage::GroupPublished)?;
    Ok((recover(store)?, written))
}

pub(super) fn publish_delta<S: PageStore>(
    store: &mut S,
    snapshot: GroupSnapshot,
    before: Option<OwnerRef>,
    level: u8,
    replace_from: usize,
) -> Result<(u32, u32)> {
    if snapshot.frontier_root.is_some()
        || snapshot.frontier_valid
        || usize::from(level) >= GROUP_DELTA_SEGMENTS
    {
        return Err(Error::InvalidState);
    }
    let mut meta = load(store, 0, PageKind::Meta)?;
    let mut state = meta.grouped_state()?;
    let written = verify_build(store, state, snapshot)?;
    let count = state.delta_count();
    if state.active.is_none()
        || state.retired.iter().any(Option::is_some)
        || replace_from > count
        || (replace_from == count && state.latest().and_then(|latest| latest.after) != before)
        || (replace_from < count
            && state.deltas[replace_from].is_none_or(|delta| delta.before != before))
    {
        return Err(Error::InvalidState);
    }
    let mut retiring = [None; GROUP_RETIRED_SEGMENTS];
    let mut retired_count = 0usize;
    for delta in state.deltas[replace_from..count].iter().flatten() {
        if let Some(old) = retired(delta.snapshot) {
            retiring[retired_count] = Some(old);
            retired_count += 1;
        }
    }
    for slot in &mut state.deltas[replace_from..] {
        *slot = None;
    }
    state.deltas[replace_from] = Some(GroupDelta {
        snapshot,
        before,
        level,
    });
    state.journal = None;
    state.retired = retiring;
    meta.set_grouped_state(state)?;
    store.commit(&[&meta])?;
    store.event(Stage::GroupPublished)?;
    Ok((recover(store)?, written))
}

fn inspect<S: PageStore>(store: &mut S, id: SegmentId, head: u32, tail: u32) -> Result<u32> {
    if head == NO_BLOCK {
        return if tail == NO_BLOCK {
            Ok(0)
        } else {
            Err(Error::InvalidState)
        };
    }
    let mut block = head;
    let mut remaining = store.blocks()?;
    let mut count = 0u32;
    loop {
        let page = load(store, block, PageKind::Grouped)?;
        if page.group_identity()?.0 != id || (block == tail && page.next()? != NO_BLOCK) {
            return Err(Error::InvalidState);
        }
        count = count.checked_add(1).ok_or(Error::Limit("group pages"))?;
        match posting_next(&page, tail, &mut remaining)? {
            Some(next) => block = next,
            None => return Ok(count),
        }
    }
}

fn shift_retired(state: &mut GroupState, index: usize) {
    for slot in index..GROUP_RETIRED_SEGMENTS - 1 {
        state.retired[slot] = state.retired[slot + 1];
    }
    state.retired[GROUP_RETIRED_SEGMENTS - 1] = None;
}

/// reclaims only unpublished or retired grouped chains under reader quiescence.
pub(super) fn recover<S: PageStore>(store: &mut S) -> Result<u32> {
    let mut reclaimed = 0u32;
    loop {
        let mut meta = load(store, 0, PageKind::Meta)?;
        let mut state = meta.grouped_state()?;
        let Some(journal) = state.journal else {
            break;
        };
        inspect(store, journal.id, journal.head, journal.tail)?;
        if state.active.is_some_and(|active| active.id == journal.id)
            || state
                .deltas
                .iter()
                .flatten()
                .any(|delta| delta.snapshot.id == journal.id)
            || state
                .retired
                .iter()
                .flatten()
                .any(|retired| retired.id == journal.id)
        {
            return Err(Error::InvalidState);
        }
        if journal.head == NO_BLOCK {
            state.journal = None;
            meta.set_grouped_state(state)?;
            store.commit(&[&meta])?;
            continue;
        }
        let page = load(store, journal.head, PageKind::Grouped)?;
        let next = if journal.head == journal.tail {
            NO_BLOCK
        } else {
            let next = page.next()?;
            if next == NO_BLOCK {
                return Err(Error::InvalidState);
            }
            next
        };
        let free = Page::free(journal.head, meta.free_head()?)?;
        meta.set_free_head(journal.head)?;
        state.journal = (next != NO_BLOCK).then_some(GroupJournal {
            head: next,
            ..journal
        });
        meta.set_grouped_state(state)?;
        store.commit(&[&meta, &free])?;
        store.event(Stage::GroupReclaimed)?;
        reclaimed = reclaimed
            .checked_add(1)
            .ok_or(Error::Limit("group recovery pages"))?;
    }
    loop {
        let mut meta = load(store, 0, PageKind::Meta)?;
        let mut state = meta.grouped_state()?;
        let Some((index, retiring)) = state
            .retired
            .iter()
            .enumerate()
            .find_map(|(index, entry)| entry.map(|entry| (index, entry)))
        else {
            return Ok(reclaimed);
        };
        inspect(store, retiring.id, retiring.head, retiring.tail)?;
        let page = load(store, retiring.head, PageKind::Grouped)?;
        let next = if retiring.head == retiring.tail {
            NO_BLOCK
        } else {
            let next = page.next()?;
            if next == NO_BLOCK {
                return Err(Error::InvalidState);
            }
            next
        };
        let free = Page::free(retiring.head, meta.free_head()?)?;
        meta.set_free_head(retiring.head)?;
        if next == NO_BLOCK {
            shift_retired(&mut state, index);
        } else {
            state.retired[index] = Some(GroupRetired {
                head: next,
                ..retiring
            });
        }
        meta.set_grouped_state(state)?;
        store.commit(&[&meta, &free])?;
        store.event(Stage::GroupReclaimed)?;
        reclaimed = reclaimed
            .checked_add(1)
            .ok_or(Error::Limit("group recovery pages"))?;
    }
}

pub(super) struct CatalogBuilder {
    entries: Vec<CatalogEntry>,
    counts: [usize; LEVELS],
    previous: Option<[u64; 2]>,
}

impl CatalogBuilder {
    pub fn new() -> Result<Self> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(LEVELS * CATALOG_ENTRIES)
            .map_err(|_| Error::Allocation)?;
        entries.resize(LEVELS * CATALOG_ENTRIES, CatalogEntry::default());
        Ok(Self {
            entries,
            counts: [0; LEVELS],
            previous: None,
        })
    }

    pub fn push<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: &mut GroupSnapshot,
        entry: CatalogEntry,
    ) -> Result<()> {
        if self.previous.is_some_and(|key| key >= entry.key) {
            return Err(Error::InvalidState);
        }
        self.previous = Some(entry.key);
        self.add(store, snapshot, 0, entry)
    }

    fn add<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: &mut GroupSnapshot,
        level: usize,
        entry: CatalogEntry,
    ) -> Result<()> {
        if level >= LEVELS {
            return Err(Error::Limit("group catalog depth"));
        }
        self.entries[level * CATALOG_ENTRIES + self.counts[level]] = entry;
        self.counts[level] += 1;
        if self.counts[level] == CATALOG_ENTRIES {
            let parent = self.flush(store, snapshot, level)?;
            self.add(store, snapshot, level + 1, parent)?;
        }
        Ok(())
    }

    fn flush<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: &mut GroupSnapshot,
        level: usize,
    ) -> Result<CatalogEntry> {
        let start = level * CATALOG_ENTRIES;
        let entries = &self.entries[start..start + self.counts[level]];
        let key = entries.first().ok_or(Error::InvalidState)?.key;
        let id = snapshot.id;
        let child = append(store, snapshot, |block| {
            Page::group_node(block, id, level as u8, entries)
        })?;
        self.counts[level] = 0;
        let mut value = [0; 64];
        value[..4].copy_from_slice(&child.to_le_bytes());
        Ok(CatalogEntry { key, value })
    }

    pub fn finish<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: &mut GroupSnapshot,
    ) -> Result<()> {
        for level in 0..LEVELS {
            if self.counts[level] == 0 {
                continue;
            }
            let higher = self.counts[level + 1..].iter().any(|&count| count != 0);
            if level > 0 && self.counts[level] == 1 && !higher {
                let entry = self.entries[level * CATALOG_ENTRIES];
                snapshot.root = child(entry)?;
                return Ok(());
            }
            let parent = self.flush(store, snapshot, level)?;
            if !higher {
                snapshot.root = child(parent)?;
                return Ok(());
            }
            self.add(store, snapshot, level + 1, parent)?;
        }
        Ok(())
    }
}

fn child(entry: CatalogEntry) -> Result<u32> {
    let block = u32::from_le_bytes(
        entry.value[..4]
            .try_into()
            .map_err(|_| Error::InvalidState)?,
    );
    if block == 0 || block == NO_BLOCK || entry.value[4..] != [0; 60] {
        return Err(Error::InvalidState);
    }
    Ok(block)
}

/// one leaf image and a bounded scalar path; no leaf-sized per-level cache.
pub(super) struct Cursor {
    leaf: Option<Page>,
    slot: u16,
    parents: [(u32, u16); LEVELS],
    depth: usize,
}

impl Cursor {
    pub fn new() -> Self {
        Self {
            leaf: None,
            slot: 0,
            parents: [(NO_BLOCK, 0); LEVELS],
            depth: 0,
        }
    }

    pub fn seek<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: GroupSnapshot,
        key: [u64; 2],
    ) -> Result<Option<CatalogEntry>> {
        if let Some(page) = &self.leaf {
            let node = page.group_node_view()?;
            let count = node.count;
            if node.key(0)? <= key && key <= node.key(count - 1)? {
                let mut lo = 0;
                let mut hi = count;
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if node.key(mid)? < key {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                self.slot = lo;
                return self.current(store, snapshot);
            }
        }
        self.leaf = None;
        self.depth = 0;
        if snapshot.root == NO_BLOCK {
            return Ok(None);
        }
        let mut block = snapshot.root;
        let mut expected = None;
        let mut lower = None;
        let mut upper = None;
        loop {
            let page = load(store, block, PageKind::Grouped)?;
            let node = page.group_node_view()?;
            let (level, count) = (node.level, node.count);
            if page.group_identity()?.0 != snapshot.id
                || expected.is_some_and(|expected| level != expected)
                || lower.is_some_and(|key| node.key(0).is_ok_and(|first| first != key))
                || upper.is_some_and(|key| node.key(count - 1).is_ok_and(|last| last >= key))
            {
                return Err(Error::InvalidState);
            }
            let mut lo = 0;
            let mut hi = count;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if node.key(mid)? < key {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if level == 0 {
                self.slot = lo;
                self.leaf = Some(page);
                return self.current(store, snapshot);
            }
            let slot = if lo < count && node.key(lo)? == key {
                lo
            } else {
                lo.saturating_sub(1)
            };
            let entry = node.entry(slot)?;
            if self.depth >= LEVELS {
                return Err(Error::InvalidState);
            }
            self.parents[self.depth] = (block, slot);
            self.depth += 1;
            lower = Some(entry.key);
            if slot + 1 < count {
                upper = Some(node.key(slot + 1)?);
            }
            expected = Some(level - 1);
            block = child(entry)?;
        }
    }

    pub fn current<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: GroupSnapshot,
    ) -> Result<Option<CatalogEntry>> {
        if let Some(page) = &self.leaf {
            if self.slot < page.group_node_info()?.1 {
                return Ok(Some(page.group_entry(self.slot)?));
            }
        } else {
            return Ok(None);
        }
        while self.depth != 0 {
            let (block, slot) = self.parents[self.depth - 1];
            let page = load(store, block, PageKind::Grouped)?;
            let (mut level, count) = page.group_node_info()?;
            if page.group_identity()?.0 != snapshot.id || level == 0 {
                return Err(Error::InvalidState);
            }
            if slot + 1 >= count {
                self.depth -= 1;
                continue;
            }
            self.parents[self.depth - 1].1 += 1;
            let mut entry = page.group_entry(slot + 1)?;
            loop {
                let next = load(store, child(entry)?, PageKind::Grouped)?;
                let (next_level, next_count) = next.group_node_info()?;
                if next.group_identity()?.0 != snapshot.id
                    || next_level + 1 != level
                    || next.group_entry(0)?.key != entry.key
                    || (slot + 2 < count
                        && next.group_entry(next_count - 1)?.key >= page.group_entry(slot + 2)?.key)
                {
                    return Err(Error::InvalidState);
                }
                level = next_level;
                if level == 0 {
                    self.leaf = Some(next);
                    self.slot = 0;
                    return Ok(Some(
                        self.leaf
                            .as_ref()
                            .ok_or(Error::InvalidState)?
                            .group_entry(0)?,
                    ));
                }
                if self.depth >= LEVELS {
                    return Err(Error::InvalidState);
                }
                self.parents[self.depth] = (next.block(), 0);
                self.depth += 1;
                entry = next.group_entry(0)?;
            }
        }
        self.leaf = None;
        Ok(None)
    }

    pub fn advance<S: PageStore>(
        &mut self,
        store: &mut S,
        snapshot: GroupSnapshot,
    ) -> Result<Option<CatalogEntry>> {
        self.slot += 1;
        self.current(store, snapshot)
    }
}

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn sparse_delta_round_trip_and_corruption() {
        let layout = HeapLayout::new(512).unwrap();
        let key = [1, 0];
        let mut pages = [
            PageOffsets {
                page: 1,
                offsets: [0; 8],
            },
            PageOffsets {
                page: 255,
                offsets: [0; 8],
            },
        ];
        pages[0].offsets[0] = (1 << 2) | (1 << 4);
        pages[1].offsets[7] = 1 << 63;
        let entry = inline_postings(key, &pages, layout).unwrap().unwrap();
        let value = Value::read(entry).unwrap();
        let (decoded, count) = value.inline_pages(layout.max_offset()).unwrap();
        assert_eq!(count, pages.len());
        for (actual, expected) in decoded[..count].iter().zip(&pages) {
            assert_eq!(actual.page, expected.page);
            assert_eq!(actual.offsets, expected.offsets);
        }

        let group = GroupKey::new(
            Generation::new(1).unwrap(),
            SegmentId::new(2).unwrap(),
            0,
            layout,
        )
        .unwrap();
        let mut expected = [0; BITMAP_BYTES];
        let mut actual = [0; BITMAP_BYTES];
        let expected_len =
            encode_bitmap(group, BitmapKind::Posting, &pages, &mut expected).unwrap();
        let actual_len =
            encode_bitmap(group, BitmapKind::Posting, &decoded[..count], &mut actual).unwrap();
        assert_eq!(&expected[..expected_len], &actual[..actual_len]);

        let mut corrupt = entry;
        corrupt.value[58] = 1;
        assert!(Value::read(corrupt).is_err());

        let mut legacy_bytes = [0u8; 20];
        legacy_bytes[..9].copy_from_slice(&value.inline.unwrap()[..9]);
        let legacy = Value {
            len: 0,
            blocks: core::array::from_fn(|index| {
                let start = index * 4;
                u32::from_le_bytes([
                    legacy_bytes[start],
                    legacy_bytes[start + 1],
                    legacy_bytes[start + 2],
                    legacy_bytes[start + 3],
                ])
            }),
            pages: value.pages,
            members: u32::from_le_bytes([
                legacy_bytes[12],
                legacy_bytes[13],
                legacy_bytes[14],
                legacy_bytes[15],
            ]),
            member_bytes: u32::from_le_bytes([
                legacy_bytes[16],
                legacy_bytes[17],
                legacy_bytes[18],
                legacy_bytes[19],
            ]),
            count: 3,
            inline: None,
        };
        assert!(Value::read(legacy.entry(key)).is_ok());
        let mut many = [PageOffsets {
            page: 1,
            offsets: [0; 8],
        }];
        many[0].offsets[0] = (1 << 18) - 1;
        assert!(inline_postings(key, &many, layout).unwrap().is_some());
        many[0].offsets[0] = (1 << 19) - 1;
        assert!(inline_postings(key, &many, layout).unwrap().is_none());
    }
}
