#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::error::{Error, Result};
use pin_core::grouped::GroupKey;
use pin_core::identity::{Generation, HeapLayout, SegmentId};
use pin_core::mutable::page::Page;
use pin_core::mutable::{PageStore, Stage};
use pin_core::primary::{Directory, Extent, PrimaryBuildWriter};

fn identity() -> (Generation, SegmentId) {
    (Generation::new(9).unwrap(), SegmentId::new(3).unwrap())
}

#[derive(Default)]
struct SerialExtendStore {
    inner: MemoryStore,
    outstanding: Option<u32>,
}

impl PageStore for SerialExtendStore {
    fn layout(&self) -> HeapLayout {
        self.inner.layout()
    }
    fn blocks(&mut self) -> Result<u32> {
        self.inner.blocks()
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        self.inner.read(block)
    }
    fn extend(&mut self) -> Result<u32> {
        if self.outstanding.is_some() {
            return Err(Error::InvalidState);
        }
        let block = self.inner.extend()?;
        self.outstanding = Some(block);
        Ok(block)
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        if pages
            .iter()
            .any(|page| self.outstanding != Some(page.block()))
        {
            return Err(Error::InvalidState);
        }
        self.inner.commit(pages)?;
        self.outstanding = None;
        Ok(())
    }
    fn interrupt(&mut self) -> Result<()> {
        self.inner.interrupt()
    }
    fn event(&mut self, stage: Stage) -> Result<()> {
        self.inner.event(stage)
    }
}

#[test]
fn sorted_runs_write_inline_and_packed_directories_before_callback() {
    let (relation, segment) = identity();
    let mut store = MemoryStore::default();
    pin_core::primary::initialize(&mut store, relation).unwrap();
    let mut writer = PrimaryBuildWriter::new(&mut store, relation, segment).unwrap();
    let mut callbacks = Vec::new();
    writer
        .push("alpha", 0, 1, &[2], |_, term, key, id, extent| {
            callbacks.push((term.to_owned(), key, id, extent));
            Ok(())
        })
        .unwrap();
    writer
        .push("alpha", 0, 2, &[3, 9], |_, term, key, id, extent| {
            callbacks.push((term.to_owned(), key, id, extent));
            Ok(())
        })
        .unwrap();
    writer
        .push("alpha", 256, 0, &[7], |_, term, key, id, extent| {
            callbacks.push((term.to_owned(), key, id, extent));
            Ok(())
        })
        .unwrap();
    assert!(callbacks.is_empty());
    assert_eq!(
        writer
            .finish(|_, term, key, id, extent| {
                callbacks.push((term.to_owned(), key, id, extent));
                Ok(())
            })
            .unwrap(),
        1
    );
    assert_eq!(callbacks.len(), 2);
    assert_eq!(callbacks[0].0, "alpha");
    assert_eq!(callbacks[0].2, 1);
    assert_eq!(callbacks[0].1.base(), 0);
    assert_eq!(callbacks[1].1.base(), 256);
    for (_, key, id, extent) in callbacks {
        let page = store.read(extent.block).unwrap();
        let bytes = page.primary_extent(extent.offset, extent.len).unwrap();
        let directory = Directory::open(bytes).unwrap();
        assert_eq!(directory.key(), key);
        assert_eq!(directory.term(), id);
    }
    assert_eq!(
        store.pages.len(),
        3,
        "one root, one posting arena and one packed directory arena"
    );
}

#[test]
fn rare_two_posting_terms_pack_postings_and_directories() {
    let (relation, segment) = identity();
    let mut store = MemoryStore::default();
    pin_core::primary::initialize(&mut store, relation).unwrap();
    let mut writer = PrimaryBuildWriter::new(&mut store, relation, segment).unwrap();
    let mut addresses: Vec<(GroupKey, u64, Extent)> = Vec::new();
    for group in 0..220u32 {
        let term = format!("rare{group:03}");
        writer
            .push(
                &term,
                group * 256,
                0,
                &[
                    u16::try_from(group % 290 + 1).unwrap(),
                    u16::try_from(group % 290 + 2).unwrap(),
                ],
                |_, _, key, id, extent| {
                    addresses.push((key, id, extent));
                    Ok(())
                },
            )
            .unwrap();
    }
    writer
        .finish(|_, _, key, id, extent| {
            addresses.push((key, id, extent));
            Ok(())
        })
        .unwrap();
    assert_eq!(addresses.len(), 220);
    let directory_blocks: std::collections::BTreeSet<_> = addresses
        .iter()
        .map(|(_, _, extent)| extent.block)
        .collect();
    assert_eq!(directory_blocks.len(), 3);
    let naive_directory_pages = addresses.len();
    let posting_blocks = (1..store.pages.len() as u32)
        .filter(|block| !directory_blocks.contains(block))
        .count();
    assert_eq!(posting_blocks, 1);
    assert_eq!(
        store.pages.len(),
        5,
        "root, one packed posting page and three packed directory pages for 220 rare terms"
    );
    assert_eq!(1 + posting_blocks + naive_directory_pages, 222);
    assert_eq!(store.pages.len(), 5);
    for (key, id, extent) in addresses {
        let page = store.read(extent.block).unwrap();
        let directory =
            Directory::open(page.primary_extent(extent.offset, extent.len).unwrap()).unwrap();
        assert_eq!(directory.key(), key);
        assert_eq!(directory.term(), id);
    }
}

#[test]
fn out_of_order_runs_poison_the_writer_and_callback_failure_aborts_finish() {
    let (relation, segment) = identity();
    let mut store = MemoryStore::default();
    pin_core::primary::initialize(&mut store, relation).unwrap();
    let mut writer = PrimaryBuildWriter::new(&mut store, relation, segment).unwrap();
    writer
        .push("beta", 0, 1, &[1], |_, _, _, _, _| Ok(()))
        .unwrap();
    assert_eq!(
        writer.push("alpha", 0, 2, &[2], |_, _, _, _, _| Ok(())),
        Err(Error::InvalidState)
    );
    assert_eq!(
        writer.finish(|_, _, _, _, _| Ok(())),
        Err(Error::InvalidState)
    );

    let mut writer = PrimaryBuildWriter::new(&mut store, relation, segment).unwrap();
    writer
        .push("a", 0, 1, &[1], |_, _, _, _, _| Ok(()))
        .unwrap();
    assert_eq!(
        writer.finish(|_, _, _, _, _| Err(Error::InvalidState)),
        Err(Error::InvalidState)
    );
}

#[test]
fn writer_rejects_a_root_for_another_relation_generation() {
    let (relation, segment) = identity();
    let mut store = MemoryStore::default();
    pin_core::primary::initialize(&mut store, relation).unwrap();
    assert!(PrimaryBuildWriter::new(&mut store, Generation::new(10).unwrap(), segment).is_err());
}

#[test]
fn alternates_posting_and_directory_pages_with_single_outstanding_extension() {
    let (relation, segment) = identity();
    let mut store = SerialExtendStore::default();
    pin_core::primary::initialize(&mut store, relation).unwrap();
    let mut writer = PrimaryBuildWriter::new(&mut store, relation, segment).unwrap();
    let mut addresses = Vec::new();
    for (term, offsets) in [
        ("a", &[1][..]),
        ("b", &[1, 3][..]),
        ("c", &[2][..]),
        ("d", &[4, 5][..]),
    ] {
        writer
            .push(term, 0, 1, offsets, |store, lexeme, key, id, extent| {
                addresses.push((lexeme.to_owned(), key, id, extent));
                let block = store.extend()?;
                let page = Page::primary(block, &[0xca])?;
                store.commit(&[&page])?;
                Ok(())
            })
            .unwrap();
    }
    writer
        .finish(|store, lexeme, key, id, extent| {
            addresses.push((lexeme.to_owned(), key, id, extent));
            let block = store.extend()?;
            let page = Page::primary(block, &[0xca])?;
            store.commit(&[&page])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(addresses.len(), 4);
    assert!(store.outstanding.is_none());
}
