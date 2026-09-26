use pin_core::error::{Error, Result};
use pin_core::grouped::GroupKey;
use pin_core::identity::{Generation, HeapLayout, RootTid, SegmentId};
use pin_core::mutable::document::MAX_TERM_BYTES;
use pin_core::mutable::page::Page;
use pin_core::mutable::{PageStore, Stage};
use pin_core::primary::{
    BuildReducer, CatalogueBuilder, CatalogueFence, CataloguePage, Directory, EncodedCataloguePage,
    GroupAddress, MAX_SORT_RECORD_BYTES, ManifestRootBuilder, PrimaryBuildWriter, TermSortRecord,
    decode_sort_record, encode_manifest_leaf, read_offsets, scan_term,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct SerialStore {
    layout: Option<HeapLayout>,
    pages: Vec<Vec<u8>>,
    outstanding: Option<u32>,
}

impl PageStore for SerialStore {
    fn layout(&self) -> HeapLayout {
        self.layout.unwrap()
    }
    fn blocks(&mut self) -> Result<u32> {
        Ok(self.pages.len() as u32)
    }
    fn read(&mut self, block: u32) -> Result<Page> {
        let bytes = self.pages.get(block as usize).ok_or(Error::InvalidState)?;
        Page::read_with(block, |out| {
            if bytes.len() > out.len() {
                return Err(Error::InvalidState);
            }
            out[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }
    fn extend(&mut self) -> Result<u32> {
        if self.outstanding.is_some() {
            return Err(Error::InvalidState);
        }
        let block = u32::try_from(self.pages.len()).map_err(|_| Error::InvalidState)?;
        self.pages.push(Vec::new());
        self.outstanding = Some(block);
        Ok(block)
    }
    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        if pages.len() != 1
            || (Some(pages[0].block()) != self.outstanding
                && (self.outstanding.is_some() || pages[0].block() != 0))
        {
            return Err(Error::InvalidState);
        }
        pages[0].validate(self.layout())?;
        self.pages[pages[0].block() as usize] = pages[0].bytes().to_vec();
        if self.outstanding == Some(pages[0].block()) {
            self.outstanding = None;
        }
        Ok(())
    }
    fn interrupt(&mut self) -> Result<()> {
        Ok(())
    }
    fn event(&mut self, _stage: Stage) -> Result<()> {
        Ok(())
    }
}

fn persist_catalogue<S: PageStore>(
    store: &mut S,
    builder: &mut CatalogueBuilder,
    term: &str,
    ordinal: u64,
    key: GroupKey,
    extent: pin_core::primary::Extent,
    catalogue_blocks: &mut Vec<u32>,
) -> Result<()> {
    let address = GroupAddress {
        group_base: key.base(),
        block: extent.block,
        offset: extent.offset,
        len: extent.len,
    };
    if builder.try_push(term.as_bytes(), ordinal, address)? {
        return Ok(());
    }
    let block = store.extend()?;
    let encoded = builder.finish_page_at(block)?;
    persist_encoded(store, encoded, catalogue_blocks)?;
    if !builder.try_push(term.as_bytes(), ordinal, address)? {
        return Err(Error::InvalidState);
    }
    Ok(())
}

fn persist_encoded<S: PageStore>(
    store: &mut S,
    encoded: EncodedCataloguePage,
    catalogue_blocks: &mut Vec<u32>,
) -> Result<()> {
    let block = encoded.fence.block;
    let page = Page::primary(block, &encoded.payload)?;
    store.commit(&[&page])?;
    catalogue_blocks.push(block);
    Ok(())
}

fn feed_run<S: PageStore>(
    writer: &mut PrimaryBuildWriter<'_, S>,
    builder: &mut CatalogueBuilder,
    catalogue_blocks: &mut Vec<u32>,
    term: &str,
    base: u32,
    page: u8,
    offsets: &[u16],
) -> Result<()> {
    writer.push(
        term,
        base,
        page,
        offsets,
        |store, term, key, ordinal, extent| {
            persist_catalogue(store, builder, term, ordinal, key, extent, catalogue_blocks)
        },
    )
}

#[test]
fn sorted_records_reduce_into_persisted_posting_directory_and_catalogue_pages() {
    let relation = Generation::new(92).unwrap();
    let segment = SegmentId::new(4).unwrap();
    let layout = HeapLayout::new(291).unwrap();
    let mut store = SerialStore {
        layout: Some(layout),
        ..SerialStore::default()
    };
    let mut root = pin_core::primary::initialize(&mut store, relation).unwrap();

    // The independent source oracle is term -> group base -> heap page -> offsets.
    let mut oracle: BTreeMap<(String, u32, u8), Vec<u16>> = BTreeMap::new();
    let mut raw_records = Vec::new();
    for group in 0..700u32 {
        let base = group * 256;
        let off = (group % 290 + 1) as u16;
        let root = RootTid::new(base, off, layout).unwrap();
        oracle.insert(("alpha".to_owned(), base, 0), vec![off]);
        raw_records.push(encode_record("alpha", root));
    }
    for (term, block, offset) in [
        ("alpha", 2, 5),
        ("alpha", 10, 9),
        ("beta", 7, 3),
        ("beta", 263, 11),
        ("gamma", 13, 1),
    ] {
        let root = RootTid::new(block, offset, layout).unwrap();
        let base = block & !255;
        let page = block as u8;
        oracle
            .entry((term.to_owned(), base, page))
            .or_default()
            .push(offset);
        raw_records.push(encode_record(term, root));
    }
    raw_records.sort_unstable();
    for offsets in oracle.values_mut() {
        offsets.sort_unstable();
    }

    let mut writer = PrimaryBuildWriter::new(&mut store, relation, segment).unwrap();
    let mut reducer = BuildReducer::new(layout).unwrap();
    let mut catalogue = CatalogueBuilder::new(1).unwrap();
    let mut catalogue_blocks = Vec::new();
    for bytes in &raw_records {
        let record = decode_sort_record(bytes, layout).unwrap();
        reducer
            .push(record, |term, base, page, offsets| {
                feed_run(
                    &mut writer,
                    &mut catalogue,
                    &mut catalogue_blocks,
                    term,
                    base,
                    page,
                    offsets,
                )
            })
            .unwrap();
    }
    let work = reducer
        .finish(|term, base, page, offsets| {
            feed_run(
                &mut writer,
                &mut catalogue,
                &mut catalogue_blocks,
                term,
                base,
                page,
                offsets,
            )
        })
        .unwrap();
    assert_eq!(work.records as usize, raw_records.len());
    writer
        .finish(|store, term, key, ordinal, extent| {
            persist_catalogue(
                store,
                &mut catalogue,
                term,
                ordinal,
                key,
                extent,
                &mut catalogue_blocks,
            )
        })
        .unwrap();
    let final_block = store.extend().unwrap();
    let final_page = catalogue.finish_page_at(final_block).unwrap();
    persist_encoded(&mut store, final_page, &mut catalogue_blocks).unwrap();
    assert_eq!(store.outstanding, None);
    assert!(
        catalogue_blocks.len() > 1,
        "fixture must roll over the catalogue page"
    );

    let publication_epoch = root.epoch.checked_add(1).unwrap();
    let mut manifest_builder =
        ManifestRootBuilder::new(relation, layout, publication_epoch).unwrap();
    for &catalogue_block in &catalogue_blocks {
        let page = store.read(catalogue_block).unwrap();
        let catalogue_page = CataloguePage::open(page.primary_payload().unwrap()).unwrap();
        let mut scratch = [0u8; MAX_TERM_BYTES];
        let (first, first_entry) = catalogue_page.entry(0, &mut scratch).unwrap();
        let first = first.to_vec();
        let first_ordinal = first_entry.term_ordinal;
        let (last, _) = catalogue_page
            .entry(
                u16::try_from(catalogue_page.len() - 1).unwrap(),
                &mut scratch,
            )
            .unwrap();
        let fence = CatalogueFence {
            first_lexeme: first,
            last_lexeme: last.to_vec(),
            first_ordinal,
            block: catalogue_block,
        };
        let leaf_block = store.extend().unwrap();
        let leaf = encode_manifest_leaf(
            relation,
            layout,
            publication_epoch,
            segment,
            leaf_block,
            &[fence],
        )
        .unwrap();
        let leaf_page = Page::primary(leaf_block, &leaf.payload).unwrap();
        store.commit(&[&leaf_page]).unwrap();
        manifest_builder.push_leaf(leaf).unwrap();
    }
    let manifest_block = store.extend().unwrap();
    let manifest = manifest_builder.finish(manifest_block).unwrap();
    let manifest_page = Page::primary(manifest_block, &manifest.root_payload).unwrap();
    store.commit(&[&manifest_page]).unwrap();
    root.epoch = publication_epoch;
    root.manifest_block = manifest_block;
    root.segment_count = 1;
    let root_page = Page::primary_metadata(root).unwrap();
    store.commit(&[&root_page]).unwrap();
    assert_eq!(store.outstanding, None);

    // Cross-check every catalogue row against its persisted group directory and offsets.
    let mut observed: BTreeMap<(String, u32, u8), Vec<u16>> = BTreeMap::new();
    let mut last_fence: Option<(Vec<u8>, Vec<u8>)> = None;
    for &catalogue_block in &catalogue_blocks {
        let image = store.read(catalogue_block).unwrap();
        let payload = image.primary_payload().unwrap();
        let catalogue_page = CataloguePage::open(payload).unwrap();
        assert_eq!(catalogue_page.block(), catalogue_block);
        let first = catalogue_page
            .entry(0, &mut [0u8; MAX_TERM_BYTES])
            .unwrap()
            .0
            .to_vec();
        if let Some((_, previous_last)) = &last_fence {
            assert!(previous_last <= &first);
        }
        let last = catalogue_page
            .entry(
                (catalogue_page.len() - 1) as u16,
                &mut [0u8; MAX_TERM_BYTES],
            )
            .unwrap()
            .0
            .to_vec();
        last_fence = Some((first, last));

        for term in [b"alpha".as_slice(), b"beta", b"gamma"] {
            let rows = catalogue_page.lookup_range(term).unwrap();
            for row in rows {
                let mut scratch = [0u8; MAX_TERM_BYTES];
                let (lexeme, entry) = catalogue_page.entry(row, &mut scratch).unwrap();
                assert_eq!(lexeme, term);
                let expected_id = match term {
                    b"alpha" => 1,
                    b"beta" => 2,
                    _ => 3,
                };
                assert_eq!(entry.term_ordinal, expected_id);
                let mut bytes = vec![0; usize::from(entry.group.len)];
                store
                    .read_primary_extent(entry.group.block, entry.group.offset, &mut bytes)
                    .unwrap();
                let directory = Directory::open(&bytes).unwrap();
                assert_eq!(directory.term(), expected_id);
                assert_eq!(directory.key().base(), entry.group.group_base);
                assert_eq!(directory.key().relation(), relation);
                assert_eq!(directory.key().segment(), segment);
                let key = directory.key();
                for (word_index, mut word) in directory.pages().into_iter().enumerate() {
                    while word != 0 {
                        let bit = word.trailing_zeros() as usize;
                        word &= word - 1;
                        let heap_page = (word_index * 64 + bit) as u8;
                        if let Some(mask) =
                            read_offsets(&mut store, key, directory, heap_page).unwrap()
                        {
                            let mut offsets = Vec::new();
                            for (word, bits) in mask.into_iter().enumerate() {
                                let mut bits = bits;
                                while bits != 0 {
                                    let bit = bits.trailing_zeros() as usize;
                                    bits &= bits - 1;
                                    offsets.push((word * 64 + bit + 1) as u16);
                                }
                            }
                            observed.insert(
                                (
                                    String::from_utf8(lexeme.to_vec()).unwrap(),
                                    key.base(),
                                    heap_page,
                                ),
                                offsets,
                            );
                        }
                    }
                }
            }
        }
    }
    assert_eq!(observed, oracle);

    for term in [b"alpha".as_slice(), b"beta", b"gamma", b"missing"] {
        let expected = oracle
            .iter()
            .filter(|((found, _, _), _)| found.as_bytes() == term)
            .flat_map(|((_, base, page), offsets)| {
                offsets.iter().map(move |offset| {
                    RootTid::new(base + u32::from(*page), *offset, layout).unwrap()
                })
            })
            .collect::<BTreeSet<_>>();
        let mut actual = Vec::new();
        let count = scan_term(&mut store, root, segment, term, |tid| {
            actual.push(tid);
            Ok(())
        })
        .unwrap();
        assert_eq!(usize::try_from(count).unwrap(), actual.len());
        actual.sort_unstable();
        assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
    }
}

fn encode_record(term: &str, root: RootTid) -> Vec<u8> {
    let mut bytes = [0u8; MAX_SORT_RECORD_BYTES];
    let len = TermSortRecord { term, root }.encode(&mut bytes).unwrap();
    bytes[..len].to_vec()
}
