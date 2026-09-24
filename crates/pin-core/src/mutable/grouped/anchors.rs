//! snapshot-time canonical suffix anchors in the grouped allocation journal.
//! source pointers are valid only until the first canonical chain rewrite.

use super::build::SortRecord;
use super::storage;
use crate::codec::bytes::Reader;
use crate::error::{Error, Result};
use crate::mutable::page::{CAPACITY, CatalogEntry, GroupSnapshot, NO_BLOCK, Page, TermRef};
use crate::mutable::{PageStore, Stage};

const MAGIC: &[u8; 4] = b"FSA1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Anchor {
    pub head: u32,
    pub tail: u32,
    pub last: u64,
}

fn term_key(reference: TermRef) -> u64 {
    (u64::from(reference.page) << 16) | u64::from(reference.offset)
}

fn valid_term(key: u64) -> bool {
    key >> 16 > 0
        && key >> 16 < u64::from(NO_BLOCK)
        && key as u16 >= 16
        && usize::from(key as u16) < CAPACITY
}

impl Anchor {
    pub fn new(head: u32, tail: u32, last: u64) -> Result<Self> {
        if last == 0
            || head == 0
            || tail == 0
            || (head == NO_BLOCK) != (tail == NO_BLOCK)
        {
            return Err(Error::InvalidState);
        }
        Ok(Self { head, tail, last })
    }

    // zero is not a valid root tid and sorts before every term posting.
    pub fn sort_record(self, term: u64) -> SortRecord {
        let mut bytes = [0; 32];
        for (chunk, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip([
            term,
            0,
            (u64::from(self.head) << 32) | u64::from(self.tail),
            self.last,
        ]) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        SortRecord(bytes)
    }

    pub fn from_sort(record: SortRecord) -> Result<Option<(u64, Self)>> {
        let words = record.0.as_chunks::<8>().0;
        if u64::from_be_bytes(words[1]) != 0 {
            return Ok(None);
        }
        let term = u64::from_be_bytes(words[0]);
        if !valid_term(term) {
            return Err(Error::InvalidState);
        }
        let chain = u64::from_be_bytes(words[2]);
        Ok(Some((
            term,
            Self::new((chain >> 32) as u32, chain as u32, u64::from_be_bytes(words[3]))?,
        )))
    }

    pub fn entry(self, term: u64) -> CatalogEntry {
        let mut value = [0; 64];
        value[..4].copy_from_slice(MAGIC);
        value[4..8].copy_from_slice(&self.head.to_le_bytes());
        value[8..12].copy_from_slice(&self.tail.to_le_bytes());
        value[12..20].copy_from_slice(&self.last.to_le_bytes());
        CatalogEntry {
            key: [term, 0],
            value,
        }
    }

    pub fn is_entry(entry: CatalogEntry) -> bool {
        entry.value[..4] == *MAGIC
    }

    pub fn read(entry: CatalogEntry) -> Result<Self> {
        if !valid_term(entry.key[0]) || entry.key[1] != 0 {
            return Err(Error::InvalidState);
        }
        let mut reader = Reader::new(&entry.value);
        if reader.take(4)? != MAGIC {
            return Err(Error::InvalidState);
        }
        let anchor = Self::new(reader.u32()?, reader.u32()?, reader.u64()?)?;
        if reader.take(44)? != [0; 44] {
            return Err(Error::InvalidState);
        }
        reader.finish()?;
        Ok(anchor)
    }
}

pub(super) fn lookup<S: PageStore>(
    store: &mut S,
    snapshot: GroupSnapshot,
    term: TermRef,
) -> Result<Anchor> {
    let root = snapshot.frontier_root.ok_or(Error::InvalidState)?;
    let key = [term_key(term), 0];
    let entry = storage::Cursor::new()
        .seek(store, GroupSnapshot { root, ..snapshot }, key)?
        .ok_or(Error::InvalidState)?;
    if entry.key != key {
        return Err(Error::InvalidState);
    }
    let anchor = Anchor::read(entry)?;
    if anchor.last >= snapshot.id.get() {
        return Err(Error::InvalidState);
    }
    Ok(anchor)
}

// the host holds the exclusive structural barrier, then the writer interlock.
// WAL must publish this bit before any source page can be replaced or recycled.
pub(crate) fn invalidate<S: PageStore>(store: &mut S, meta: &mut Page) -> Result<()> {
    let mut state = meta.grouped_state()?;
    if let Some(mut active) = state.active
        && active.frontier_valid
    {
        active.frontier_valid = false;
        state.active = Some(active);
        meta.set_grouped_state(state)?;
        store.commit(&[meta])?;
        store.event(Stage::FrontierInvalidated)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_records_round_trip_and_reject_corruption() {
        let term = (1 << 16) | 16;
        for anchor in [Anchor::new(7, 3, 99).unwrap(), Anchor::new(NO_BLOCK, NO_BLOCK, 1).unwrap()] {
            assert_eq!(Anchor::from_sort(anchor.sort_record(term)).unwrap(), Some((term, anchor)));
            assert_eq!(Anchor::read(anchor.entry(term)).unwrap(), anchor);
            for index in [0, 20, 63] {
                let mut bad = anchor.entry(term);
                bad.value[index] ^= 1;
                assert!(Anchor::read(bad).is_err());
            }
        }
        for (head, tail, last) in [(0, 1, 1), (1, NO_BLOCK, 1), (NO_BLOCK, 1, 1), (1, 1, 0)] {
            assert!(Anchor::new(head, tail, last).is_err());
        }
        assert!(Anchor::from_sort(SortRecord::default()).is_err());
    }
}
