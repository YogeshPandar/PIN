use super::{CataloguePage, Directory, MAX_DIRECTORY_BYTES, PrimaryRoot, read_manifest};
use crate::error::{Error, Result};
use crate::grouped::GroupKey;
use crate::identity::{RootTid, SegmentId};
use crate::mutable::PageStore;
use crate::mutable::document::MAX_TERM_BYTES;

/// Exact lexeme lookup over a published immutable v2 segment. Heap visibility
/// remains the PostgreSQL bitmap heap scan's responsibility.
pub fn scan_term<S: PageStore>(
    store: &mut S,
    root: PrimaryRoot,
    segment: SegmentId,
    term: &[u8],
    mut emit: impl FnMut(RootTid) -> Result<()>,
) -> Result<u64> {
    if term.is_empty() || term.len() > MAX_TERM_BYTES || root.layout != store.layout() {
        return Err(Error::InvalidParameters);
    }
    if root.segment_count == 0 {
        return Ok(0);
    }
    let manifest = read_manifest(store, root)?;
    let fences = manifest.locate_fences(store, segment, term, false)?;
    let mut count = 0u64;
    let mut prior_ordinal = None;
    let mut prior_base = None;
    let mut posting_page = None;
    for fence in fences {
        store.interrupt()?;
        let page = store.read(fence.block)?;
        page.validate(root.layout)?;
        let catalogue = CataloguePage::open(page.primary_payload()?)?;
        if catalogue.block() != fence.block || catalogue.is_empty() {
            return Err(Error::InvalidState);
        }
        let mut scratch = [0u8; MAX_TERM_BYTES];
        let (first, first_entry) = catalogue.entry(0, &mut scratch)?;
        if first != fence.first_lexeme || first_entry.term_ordinal != fence.first_ordinal {
            return Err(Error::InvalidState);
        }
        let (last, _) = catalogue.entry(
            u16::try_from(catalogue.len() - 1).map_err(|_| Error::InvalidState)?,
            &mut scratch,
        )?;
        if last != fence.last_lexeme {
            return Err(Error::InvalidState);
        }
        catalogue.visit_exact(term, |found, entry| {
            if found != term
                || prior_ordinal.is_some_and(|old| old != entry.term_ordinal)
                || prior_base.is_some_and(|old| old >= entry.group.group_base)
                || usize::from(entry.group.len) > MAX_DIRECTORY_BYTES
            {
                return Err(Error::InvalidState);
            }
            prior_ordinal = Some(entry.term_ordinal);
            prior_base = Some(entry.group.group_base);
            let key = GroupKey::new(root.relation, segment, entry.group.group_base, root.layout)?;
            let mut bytes = [0u8; MAX_DIRECTORY_BYTES];
            let length = usize::from(entry.group.len);
            store.read_primary_extent(
                entry.group.block,
                entry.group.offset,
                &mut bytes[..length],
            )?;
            let directory = Directory::open(&bytes[..length])?;
            if directory.key() != key || directory.term() != entry.term_ordinal {
                return Err(Error::InvalidState);
            }
            for (word, mut pages) in directory.pages().into_iter().enumerate() {
                while pages != 0 {
                    let bit = pages.trailing_zeros() as u8;
                    pages &= pages - 1;
                    let page_number = word as u8 * 64 + bit;
                    let mask = directory
                        .offsets(page_number, |extent, output| {
                            if posting_page.as_ref().is_none_or(
                                |page: &crate::mutable::page::Page| page.block() != extent.block,
                            ) {
                                let page = store.read(extent.block)?;
                                page.validate(root.layout)?;
                                posting_page = Some(page);
                            }
                            let page = posting_page.as_ref().ok_or(Error::InvalidState)?;
                            output.copy_from_slice(page.primary_extent(extent.offset, extent.len)?);
                            Ok(())
                        })?
                        .ok_or(Error::InvalidState)?;
                    let heap_block = key.base() | u32::from(page_number);
                    for (offset_word, mut offsets) in mask.into_iter().enumerate() {
                        while offsets != 0 {
                            let bit = offsets.trailing_zeros() as u16;
                            offsets &= offsets - 1;
                            let offset = offset_word as u16 * 64 + bit + 1;
                            emit(
                                RootTid::new(heap_block, offset, root.layout)
                                    .map_err(|_| Error::InvalidState)?,
                            )?;
                            count = count
                                .checked_add(1)
                                .ok_or(Error::Limit("scan candidates"))?;
                        }
                    }
                }
            }
            Ok(())
        })?;
    }
    Ok(count)
}
