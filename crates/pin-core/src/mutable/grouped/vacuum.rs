//! shared liveness retirement before canonical owner removal and heap reuse.

use super::super::page::{GROUP_DATA_BYTES, GroupPageKind, Page, PageKind};
use super::super::{PageStore, Stage, load};
use super::storage::{self, BITMAP_BYTES, Cursor, Value};
use crate::error::{Error, Result};
use crate::grouped::retire_roots;
use crate::identity::RootTid;

fn retire_snapshot<S: PageStore>(
    store: &mut S,
    snapshot: super::super::page::GroupSnapshot,
    removable: &mut impl FnMut(RootTid) -> Result<bool>,
) -> Result<u64> {
    let meta = load(store, 0, PageKind::Meta)?;
    let mut cursor = Cursor::new();
    let mut entry = cursor.seek(store, snapshot, [0, 0])?;
    let mut bytes = [0; BITMAP_BYTES];
    let mut total = 0u64;
    while let Some(current) = entry.filter(|entry| entry.key[0] == 0) {
        let value = storage::read_bitmap(store, snapshot, current, &mut bytes)?;
        let key = storage::key(snapshot, current.key[1] as u32, store.layout())?;
        let bytes = &mut bytes[..value.len as usize];
        let removed = retire_roots(bytes, key, &mut *removable)?;
        if removed != 0 {
            let mut changes: [Option<Page>; 3] = core::array::from_fn(|_| None);
            let mut used = 0;
            for (index, chunk) in bytes.chunks(GROUP_DATA_BYTES).enumerate() {
                let old = load(store, value.blocks[index], PageKind::Grouped)?;
                let data = old.group_data()?;
                if data.id != snapshot.id
                    || data.kind != GroupPageKind::Liveness
                    || data.key != current.key
                    || data.total != value.len
                    || data.offset as usize != index * GROUP_DATA_BYTES
                    || data.bytes.len() != chunk.len()
                {
                    return Err(Error::InvalidState);
                }
                // writer exclusion prevents a stale image from restoring another clear.
                if chunk
                    .iter()
                    .zip(data.bytes)
                    .any(|(&new, &old)| new & !old != 0)
                {
                    return Err(Error::InvalidState);
                }
                if chunk != data.bytes {
                    let mut page = Page::group_record(
                        old.block(),
                        snapshot.id,
                        GroupPageKind::Liveness,
                        current.key,
                        value.len,
                        data.offset,
                        chunk,
                    )?;
                    page.set_next(old.next()?)?;
                    changes[used] = Some(page);
                    used += 1;
                }
            }
            let mut refs = [&meta; 3];
            for (slot, page) in refs.iter_mut().zip(changes.iter().flatten()) {
                *slot = page;
            }
            if used == 0 {
                return Err(Error::InvalidState);
            }
            store.commit(&refs[..used])?;
            store.event(Stage::GroupRetired)?;
            total = total
                .checked_add(u64::from(removed))
                .ok_or(Error::Limit("group retired roots"))?;
        }
        entry = cursor.advance(store, snapshot)?;
    }
    Ok(total)
}

pub(super) fn retire<S: PageStore>(
    store: &mut S,
    mut removable: impl FnMut(RootTid) -> Result<bool>,
) -> Result<u64> {
    let meta = load(store, 0, PageKind::Meta)?;
    let state = meta.grouped_state()?;
    let Some(active) = state.active else {
        return Ok(0);
    };
    let mut total = retire_snapshot(store, active, &mut removable)?;
    for delta in state.deltas.iter().flatten() {
        total = total
            .checked_add(retire_snapshot(store, delta.snapshot, &mut removable)?)
            .ok_or(Error::Limit("group retired roots"))?;
    }
    Ok(total)
}

pub(super) fn references(page: &Page, target: u32) -> Result<bool> {
    if page.kind() == PageKind::Meta {
        let state = page.grouped_state()?;
        return Ok(state.active.is_some_and(|active| {
            [active.head, active.tail, active.root].contains(&target)
                || active.frontier_root == Some(target)
                || active.after.is_some_and(|owner| owner.page == target)
        }) || state.deltas.iter().flatten().any(|delta| {
            let snapshot = delta.snapshot;
            [snapshot.head, snapshot.tail, snapshot.root].contains(&target)
                || snapshot.after.is_some_and(|owner| owner.page == target)
                || delta.before.is_some_and(|owner| owner.page == target)
        }) || state
            .journal
            .is_some_and(|journal| [journal.head, journal.tail].contains(&target))
            || state
                .retired
                .iter()
                .flatten()
                .any(|retired| [retired.head, retired.tail].contains(&target)));
    }
    if page.kind() != PageKind::Grouped {
        return Ok(false);
    }
    match page.group_identity()?.1 {
        GroupPageKind::Leaf | GroupPageKind::Branch => {
            let (level, count) = page.group_node_info()?;
            for index in 0..count {
                let entry = page.group_entry(index)?;
                if entry.key[0] >> 16 == u64::from(target) {
                    return Ok(true);
                }
                if level != 0 {
                    if u32::from_le_bytes(
                        entry.value[..4]
                            .try_into()
                            .map_err(|_| Error::InvalidState)?,
                    ) == target
                    {
                        return Ok(true);
                    }
                } else if super::anchors::Anchor::is_entry(entry) {
                    // compaction invalidates these advisory pointers before reuse.
                    super::anchors::Anchor::read(entry)?;
                } else {
                    let value = Value::read(entry)?;
                    if value.len != 0 && (value.blocks.contains(&target) || value.members == target)
                    {
                        return Ok(true);
                    }
                }
            }
        }
        _ => {
            if page.group_data()?.key[0] >> 16 == u64::from(target) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
