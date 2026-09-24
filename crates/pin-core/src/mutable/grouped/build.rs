//! complete-owner capture and bounded grouped snapshot construction.
//! the host sort spills fixed-width records; legacy storage remains the oracle.

use super::super::page::{BUCKETS, GroupPageKind, NO_BLOCK, OwnerRef, PageKind, TermRef};
use super::super::{PageStore, following, load, load_posting, posting_next};
use super::anchors::Anchor;
use super::storage::{self, BITMAP_BYTES, CATALOG_MEMORY, CatalogBuilder, MEMBER_BYTES, Value};
use crate::codec::records::Publication;
use crate::error::{Error, Result};
use crate::grouped::{
    Bitmap, BitmapKind, Member, Members, PageOffsets, encode_bitmap, encode_members,
};
use crate::identity::{HeapLayout, Incarnation, RootTid};

pub const SORT_BATCH: usize = 256;

/// lexicographic big-endian key: term, root, incarnation, canonical owner slot.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct SortRecord(pub [u8; 32]);

impl SortRecord {
    fn new(term: u64, root: RootTid, owner: OwnerRef) -> Self {
        let mut bytes = [0; 32];
        for (chunk, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip([
            term,
            root.key(),
            owner.incarnation.get(),
            (u64::from(owner.page) << 16) | u64::from(owner.slot),
        ]) {
            chunk.copy_from_slice(&value.to_be_bytes());
        }
        Self(bytes)
    }

    fn fields(self, layout: HeapLayout) -> Result<(u64, Member)> {
        let mut values = [0; 4];
        for (value, bytes) in values.iter_mut().zip(self.0.as_chunks::<8>().0) {
            *value = u64::from_be_bytes(*bytes);
        }
        if values[1] >> 16 >= u64::from(NO_BLOCK)
            || values[3] >> 16 == 0
            || values[3] >> 16 >= u64::from(NO_BLOCK)
        {
            return Err(Error::InvalidState);
        }
        let root = RootTid::new((values[1] >> 16) as u32, values[1] as u16, layout)
            .map_err(|_| Error::InvalidState)?;
        let incarnation = Incarnation::new(values[2]).map_err(|_| Error::InvalidState)?;
        Ok((values[0], Member { root, incarnation }))
    }
}

/// fresh host-owned sort; returns every submitted record unchanged, once, in order.
/// `finish` transitions from insertion to reading. errors abort the host operation.
/// the host bounds resident memory, owns spill files and cleans up on every exit.
pub trait GroupSort {
    fn put(&mut self, records: &[SortRecord]) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
    fn read(&mut self, output: &mut [SortRecord]) -> Result<usize>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BuildStats {
    pub documents: u64,
    pub postings: u64,
    pub frontier_terms: u64,
    pub groups: u32,
    pub term_groups: u64,
    pub reclaimed_pages: u32,
    pub written_pages: u32,
    pub reused_pages: u32,
}

struct Batch {
    rows: [SortRecord; SORT_BATCH],
    len: usize,
}

impl Batch {
    fn new() -> Self {
        Self {
            rows: [SortRecord::default(); SORT_BATCH],
            len: 0,
        }
    }
    fn push<T: GroupSort>(&mut self, sort: &mut T, record: SortRecord) -> Result<()> {
        self.rows[self.len] = record;
        self.len += 1;
        if self.len == SORT_BATCH {
            self.flush(sort)?;
        }
        Ok(())
    }
    fn flush<T: GroupSort>(&mut self, sort: &mut T) -> Result<()> {
        if self.len != 0 {
            sort.put(&self.rows[..self.len])?;
            self.len = 0;
        }
        Ok(())
    }
}

fn term_key(reference: TermRef) -> u64 {
    (u64::from(reference.page) << 16) | u64::from(reference.offset)
}

fn capture<S: PageStore, T: GroupSort>(
    store: &mut S,
    sort: &mut T,
) -> Result<(BuildStats, Option<OwnerRef>)> {
    let meta = load(store, 0, PageKind::Meta)?;
    let mut batch = Batch::new();
    let mut stats = BuildStats::default();
    let mut expected_postings = 0u64;
    let mut after = None;
    let (head, tail) = meta.owner_chain()?;
    if head != NO_BLOCK {
        let mut block = head;
        loop {
            let page = load(store, block, PageKind::Owners)?;
            for slot in 0..page.owner_count()? {
                let owner = page.owner(slot, store.layout())?;
                if after.is_some_and(|previous: OwnerRef| {
                    previous.incarnation >= owner.reference.incarnation
                }) {
                    return Err(Error::InvalidState);
                }
                after = Some(owner.reference);
                if owner.publication == Publication::Published && owner.live {
                    batch.push(sort, SortRecord::new(0, owner.root, owner.reference))?;
                    stats.documents += 1;
                    expected_postings = expected_postings
                        .checked_add(u64::from(owner.terms))
                        .ok_or(Error::Limit("group postings"))?;
                }
            }
            match following(&page, tail)? {
                Some(next) => block = next,
                None => break,
            }
        }
    }
    let mut cache = None;
    for bucket in 0..BUCKETS {
        let (head, tail) = meta.bucket(bucket)?;
        if head == NO_BLOCK {
            continue;
        }
        let mut block = head;
        loop {
            let dictionary = load(store, block, PageKind::Dictionary)?;
            for entry in dictionary.terms()? {
                let entry = entry?;
                if super::super::page::bucket_for(entry.term) != bucket {
                    return Err(Error::InvalidState);
                }
                let key = term_key(entry.reference);
                if let Some(root) = super::super::reader::resolve(store, &mut cache, entry.first)? {
                    batch.push(sort, SortRecord::new(key, root, entry.first))?;
                    stats.postings += 1;
                }
                let mut previous = entry.first;
                if entry.head == NO_BLOCK {
                    if store.frontier_anchors() {
                        batch.push(
                            sort,
                            Anchor::new(entry.head, entry.tail, previous.incarnation.get())?
                                .sort_record(key),
                        )?;
                        stats.frontier_terms = stats
                            .frontier_terms
                            .checked_add(1)
                            .ok_or(Error::Limit("frontier terms"))?;
                    }
                    continue;
                }
                let mut block = entry.head;
                let mut remaining = store.blocks()?;
                loop {
                    let page = load_posting(store, block, entry.reference)?;
                    let before = previous;
                    for owner in page.posting_refs()? {
                        let owner = owner?;
                        if (owner.page, owner.slot) <= (previous.page, previous.slot)
                            || owner.incarnation <= previous.incarnation
                        {
                            return Err(Error::InvalidState);
                        }
                        previous = owner;
                        if let Some(root) = super::super::reader::resolve(store, &mut cache, owner)?
                        {
                            batch.push(sort, SortRecord::new(key, root, owner))?;
                            stats.postings += 1;
                        }
                    }
                    if store.frontier_anchors() && block == entry.tail && previous == before {
                        return Err(Error::InvalidState);
                    }
                    match posting_next(&page, entry.tail, &mut remaining)? {
                        Some(next) => block = next,
                        None => break,
                    }
                }
                if store.frontier_anchors() {
                    batch.push(
                        sort,
                        Anchor::new(entry.head, entry.tail, previous.incarnation.get())?
                            .sort_record(key),
                    )?;
                    stats.frontier_terms = stats
                        .frontier_terms
                        .checked_add(1)
                        .ok_or(Error::Limit("frontier terms"))?;
                }
            }
            match following(&dictionary, tail)? {
                Some(next) => block = next,
                None => break,
            }
        }
    }
    if stats.postings != expected_postings {
        return Err(Error::InvalidState);
    }
    batch.flush(sort)?;
    Ok((stats, after))
}

fn buffer<T: Clone>(capacity: usize, value: T) -> Result<Vec<T>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Allocation)?;
    result.resize(capacity, value);
    Ok(result)
}

/// reserves bounded core scratch separately from the host sort budget.
/// the checked heap layout bounds all arithmetic independently of relation size.
pub fn build_memory(layout: HeapLayout) -> usize {
    let limit = 256 * usize::from(layout.max_offset());
    MEMBER_BYTES
        + limit * core::mem::size_of::<Member>()
        + 256 * core::mem::size_of::<PageOffsets>()
        + CATALOG_MEMORY
        + 128 * 1024
}

/// reserves one additional bounded catalog for opt-in frontier anchors.
pub fn build_memory_with_anchors(layout: HeapLayout) -> usize {
    build_memory(layout) + CATALOG_MEMORY
}

/// tests the append-only owner cutoff while holding both maintenance barriers.
/// deletions alone only clear shared liveness and do not require rebuilding.
///
/// # errors
/// rejects malformed metadata or a missing captured owner anchor.
pub fn needs_rebuild<S: PageStore>(store: &mut S) -> Result<bool> {
    let meta = load(store, 0, PageKind::Meta)?;
    let Some(snapshot) = meta.grouped_state()?.active else {
        return Ok(true);
    };
    let (_, tail) = meta.owner_chain()?;
    let after = if tail == NO_BLOCK {
        None
    } else {
        let owners = load(store, tail, PageKind::Owners)?;
        let count = owners.owner_count()?;
        Some(
            owners
                .owner(
                    count.checked_sub(1).ok_or(Error::InvalidState)?,
                    store.layout(),
                )?
                .reference,
        )
    };
    if let Some(old) = snapshot.after
        && (after.is_none_or(|new| old.incarnation > new.incarnation)
            || load(store, old.page, PageKind::Owners)?
                .owner(old.slot, store.layout())?
                .reference
                != old)
    {
        return Err(Error::InvalidState);
    }
    Ok(snapshot.after != after || (store.frontier_anchors() && !snapshot.frontier_valid))
}

/// builds a complete supplemental snapshot under exclusive reader and writer barriers.
/// no legacy page is removed. publication changes only grouped metadata.
/// resident core scratch is bounded by `memory_bytes`; host sort memory is separate.
///
/// # errors
/// rejects incomplete coverage, duplicate live coordinates, corruption and host errors.
/// errors leave the old snapshot active or the complete new snapshot published.
/// recovery frees interrupted output; callers must not retry using the consumed sort.
pub fn rebuild<S: PageStore, T: GroupSort>(
    store: &mut S,
    sort: &mut T,
    memory_bytes: usize,
) -> Result<BuildStats> {
    let limit = 256 * usize::from(store.layout().max_offset());
    let required = if store.frontier_anchors() {
        build_memory_with_anchors(store.layout())
    } else {
        build_memory(store.layout())
    };
    if memory_bytes < required {
        return Err(Error::Limit("group build scratch"));
    }
    let recovered = storage::recover(store)?
        .checked_add(super::super::recover_compaction(store)?)
        .ok_or(Error::Limit("group recovery pages"))?;
    let (mut stats, after) = capture(store, sort)?;
    stats.reclaimed_pages = recovered;
    sort.finish()?;
    store.event(super::super::Stage::GroupSortReady)?;
    let before = store.blocks()?;
    let mut snapshot = storage::begin(store, after)?;
    let mut catalog = CatalogBuilder::new()?;
    let mut anchors = if store.frontier_anchors() {
        Some(CatalogBuilder::new()?)
    } else {
        None
    };
    let mut bytes = buffer(MEMBER_BYTES, 0u8)?;
    let mut members = Vec::new();
    members
        .try_reserve_exact(limit)
        .map_err(|_| Error::Allocation)?;
    let mut pages: Vec<PageOffsets> = Vec::new();
    pages
        .try_reserve_exact(256)
        .map_err(|_| Error::Allocation)?;
    let mut rows = [SortRecord::default(); SORT_BATCH];
    let mut current = None;
    let mut previous = None;
    let mut previous_root = None;
    let mut seen = 0u64;
    let mut frontier_terms = 0u64;
    let mut frontier_term = None;
    loop {
        let count = sort.read(&mut rows)?;
        if count > rows.len() {
            return Err(Error::InvalidState);
        }
        if count == 0 {
            break;
        }
        for &row in &rows[..count] {
            store.interrupt()?;
            if previous.is_some_and(|previous| previous >= row) {
                return Err(Error::InvalidState);
            }
            previous = Some(row);
            if let Some((term, anchor)) = Anchor::from_sort(row)? {
                if anchor.last >= snapshot.id.get() {
                    return Err(Error::InvalidState);
                }
                anchors.as_mut().ok_or(Error::InvalidState)?.push(
                    store,
                    &mut snapshot,
                    anchor.entry(term),
                )?;
                frontier_term = Some(term);
                frontier_terms = frontier_terms
                    .checked_add(1)
                    .ok_or(Error::Limit("frontier terms"))?;
                continue;
            }
            let (term, member) = row.fields(store.layout())?;
            if (anchors.is_some() && term != 0 && frontier_term != Some(term))
                || member.incarnation.get() >= snapshot.id.get()
            {
                return Err(Error::InvalidState);
            }
            let key = [term, u64::from(member.root.block() & !255)];
            if current != Some(key) {
                if let Some(key) = current {
                    seal(
                        store,
                        &mut snapshot,
                        &mut catalog,
                        key,
                        &members,
                        &pages,
                        &mut bytes,
                        &mut stats,
                    )?;
                }
                members.clear();
                pages.clear();
                previous_root = None;
                current = Some(key);
            }
            if previous_root == Some(member.root) {
                return Err(Error::DuplicateDocument);
            }
            previous_root = Some(member.root);
            if term == 0 {
                if members.len() == limit {
                    return Err(Error::Limit("group members"));
                }
                members.push(member);
            } else {
                let page = member.root.block() as u8;
                if pages.last().is_none_or(|last| last.page != page) {
                    pages.push(PageOffsets {
                        page,
                        offsets: [0; 8],
                    });
                }
                let bit = usize::from(member.root.offset() - 1);
                pages.last_mut().ok_or(Error::InvalidState)?.offsets[bit / 64] |= 1 << (bit % 64);
            }
            seen = seen
                .checked_add(1)
                .ok_or(Error::Limit("group sort records"))?;
        }
    }
    if frontier_terms != stats.frontier_terms {
        return Err(Error::InvalidState);
    }
    if seen
        != stats
            .documents
            .checked_add(stats.postings)
            .ok_or(Error::Limit("group records"))?
    {
        return Err(Error::InvalidState);
    }
    if let Some(key) = current {
        seal(
            store,
            &mut snapshot,
            &mut catalog,
            key,
            &members,
            &pages,
            &mut bytes,
            &mut stats,
        )?;
    }
    catalog.finish(store, &mut snapshot)?;
    if let Some(mut anchors) = anchors {
        let root = snapshot.root;
        snapshot.root = NO_BLOCK;
        anchors.finish(store, &mut snapshot)?;
        snapshot.frontier_root = Some(snapshot.root);
        snapshot.frontier_valid = true;
        snapshot.root = root;
    }
    let (reclaimed, written) = storage::publish(store, snapshot)?;
    let extended = store
        .blocks()?
        .checked_sub(before)
        .ok_or(Error::InvalidState)?;
    stats.reclaimed_pages = stats
        .reclaimed_pages
        .checked_add(reclaimed)
        .ok_or(Error::Limit("group pages"))?;
    stats.written_pages = written;
    stats.reused_pages = written.checked_sub(extended).ok_or(Error::InvalidState)?;
    Ok(stats)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one bounded sealing workspace serves both record kinds"
)]
fn seal<S: PageStore>(
    store: &mut S,
    snapshot: &mut super::super::page::GroupSnapshot,
    catalog: &mut CatalogBuilder,
    key: [u64; 2],
    members: &[Member],
    pages: &[PageOffsets],
    bytes: &mut [u8],
    stats: &mut BuildStats,
) -> Result<()> {
    let logical_key = storage::key(*snapshot, key[1] as u32, store.layout())?;
    let (len, members_head, member_bytes, count, kind) = if key[0] == 0 {
        let member_bytes = encode_members(logical_key, members, bytes)?;
        let (head, _) = storage::write_record(
            store,
            snapshot,
            GroupPageKind::Members,
            key,
            &bytes[..member_bytes],
        )?;
        let roster = Members::open(&bytes[..member_bytes])?;
        // liveness uses separate bounded scratch while the roster borrows its bytes.
        let mut live = [0; BITMAP_BYTES];
        let len = roster.encode_liveness(&mut live)?;
        bytes[..len].copy_from_slice(&live[..len]);
        stats.groups += 1;
        (
            len,
            head,
            member_bytes as u32,
            members.len() as u32,
            GroupPageKind::Liveness,
        )
    } else {
        let len = encode_bitmap(logical_key, BitmapKind::Posting, pages, bytes)?;
        stats.term_groups += 1;
        (len, NO_BLOCK, 0, 0, GroupPageKind::Posting)
    };
    let mask = *Bitmap::open(&bytes[..len])?.pages();
    let (_, blocks) = storage::write_record(store, snapshot, kind, key, &bytes[..len])?;
    catalog.push(
        store,
        snapshot,
        Value {
            len: len as u32,
            blocks,
            pages: mask,
            members: members_head,
            member_bytes,
            count,
        }
        .entry(key),
    )
}
