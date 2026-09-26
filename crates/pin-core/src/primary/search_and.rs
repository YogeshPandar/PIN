//! bounded streaming conjunction over immutable catalogue terms.

use super::{
    CataloguePage, Directory, GroupAddress, MAX_DIRECTORY_BYTES, ManifestPageFence, PrimaryRoot,
    read_manifest, scan_and,
};
use crate::error::{Error, Result};
use crate::grouped::GroupKey;
use crate::identity::{RootTid, SegmentId};
use crate::mutable::PageStore;
use crate::mutable::document::MAX_TERM_BYTES;

const MAX_TERMS: usize = 32;
const MAX_GROUP_ADDRESSES: usize = 65_536;

struct TermGroups {
    ordinal: Option<u64>,
    addresses: Vec<GroupAddress>,
}

fn collect_term_groups<S: PageStore>(
    store: &mut S,
    root: PrimaryRoot,
    term: &[u8],
    fences: Vec<ManifestPageFence>,
    remaining: &mut usize,
) -> Result<TermGroups> {
    let mut groups = Vec::new();
    let mut ordinal = None;
    let mut prior_base = None;
    for fence in fences {
        store.interrupt()?;
        let page = store.read(fence.block)?;
        page.validate(root.layout)?;
        if page.block() != fence.block {
            return Err(Error::InvalidState);
        }
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
        for row in catalogue.lookup_range(term)? {
            let (found, entry) = catalogue.entry(row, &mut scratch)?;
            if found != term
                || ordinal.is_some_and(|value| value != entry.term_ordinal)
                || prior_base.is_some_and(|base| base >= entry.group.group_base)
                || usize::from(entry.group.len) > MAX_DIRECTORY_BYTES
            {
                return Err(Error::InvalidState);
            }
            *remaining = remaining
                .checked_sub(1)
                .ok_or(Error::Limit("AND group addresses"))?;
            groups.try_reserve(1).map_err(|_| Error::Allocation)?;
            groups.push(entry.group);
            ordinal = Some(entry.term_ordinal);
            prior_base = Some(entry.group.group_base);
        }
    }
    Ok(TermGroups {
        ordinal,
        addresses: groups,
    })
}

/// emits exact conjunction candidates in ascending RootTid order.
///
/// Directory group addresses are capped at 65,536 across all terms. Exceeding
/// that bound returns a resource error before emitting any candidate. Output is
/// streamed one matching group at a time; an error invalidates prior callback
/// output and the caller must discard it.
pub fn scan_terms_and<S: PageStore>(
    store: &mut S,
    root: PrimaryRoot,
    segment: SegmentId,
    terms: &[&[u8]],
    mut emit: impl FnMut(RootTid) -> Result<()>,
) -> Result<u64> {
    if terms.is_empty() || terms.len() > MAX_TERMS || root.layout != store.layout() {
        return Err(Error::InvalidParameters);
    }
    if root.segment_count == 0 {
        return if root.manifest_block == crate::mutable::page::NO_BLOCK {
            Ok(0)
        } else {
            Err(Error::InvalidState)
        };
    }
    let mut ordered_terms = Vec::new();
    ordered_terms
        .try_reserve_exact(terms.len())
        .map_err(|_| Error::Allocation)?;
    for term in terms {
        if term.is_empty() || term.len() > MAX_TERM_BYTES {
            return Err(Error::InvalidParameters);
        }
        ordered_terms.push(*term);
    }
    ordered_terms.sort_unstable();
    ordered_terms.dedup();

    let manifest = read_manifest(store, root)?;
    let mut remaining = MAX_GROUP_ADDRESSES;
    let mut groups = Vec::new();
    groups
        .try_reserve_exact(ordered_terms.len())
        .map_err(|_| Error::Allocation)?;
    for term in ordered_terms {
        let fences = manifest.locate_fences(store, segment, term, false)?;
        let term_groups = collect_term_groups(store, root, term, fences, &mut remaining)?;
        if term_groups.addresses.is_empty() {
            return Ok(0);
        }
        groups.push(term_groups);
    }

    let mut positions = [0usize; MAX_TERMS];
    let directory_bytes_len = groups
        .len()
        .checked_mul(MAX_DIRECTORY_BYTES)
        .ok_or(Error::InvalidState)?;
    let mut directory_bytes = Vec::new();
    directory_bytes
        .try_reserve_exact(directory_bytes_len)
        .map_err(|_| Error::Allocation)?;
    directory_bytes.resize(directory_bytes_len, 0);
    let mut directory_lengths = Vec::new();
    directory_lengths
        .try_reserve_exact(groups.len())
        .map_err(|_| Error::Allocation)?;
    directory_lengths.resize(groups.len(), 0usize);
    let mut emitted = 0u64;
    loop {
        if groups
            .iter()
            .enumerate()
            .any(|(i, term)| positions[i] >= term.addresses.len())
        {
            break;
        }
        let target_base = groups
            .iter()
            .enumerate()
            .map(|(i, term)| term.addresses[positions[i]].group_base)
            .max()
            .ok_or(Error::InvalidState)?;
        let mut aligned = true;
        for (i, term) in groups.iter().enumerate() {
            while positions[i] < term.addresses.len()
                && term.addresses[positions[i]].group_base < target_base
            {
                positions[i] += 1;
            }
            if positions[i] == term.addresses.len() {
                return Ok(emitted);
            }
            if term.addresses[positions[i]].group_base != target_base {
                aligned = false;
            }
        }
        if !aligned {
            continue;
        }

        let key = GroupKey::new(root.relation, segment, target_base, root.layout)?;
        for (index, term) in groups.iter().enumerate() {
            let address = term.addresses[positions[index]];
            let length = usize::from(address.len);
            if length > MAX_DIRECTORY_BYTES {
                return Err(Error::InvalidState);
            }
            let start = index * MAX_DIRECTORY_BYTES;
            store.read_primary_extent(
                address.block,
                address.offset,
                &mut directory_bytes[start..start + length],
            )?;
            directory_lengths[index] = length;
        }
        {
            let mut directories = Vec::<Directory<'_>>::new();
            directories
                .try_reserve_exact(groups.len())
                .map_err(|_| Error::Allocation)?;
            for (index, term) in groups.iter().enumerate() {
                let start = index * MAX_DIRECTORY_BYTES;
                let directory =
                    Directory::open(&directory_bytes[start..start + directory_lengths[index]])?;
                if directory.key() != key
                    || directory.term() != term.ordinal.ok_or(Error::InvalidState)?
                {
                    return Err(Error::InvalidState);
                }
                directories.push(directory);
            }

            store.interrupt()?;
            scan_and(store, key, &directories, |block, mask| {
                for (word_index, mut word) in mask.into_iter().enumerate() {
                    while word != 0 {
                        let bit = word.trailing_zeros() as u16;
                        word &= word - 1;
                        let offset = word_index as u16 * 64 + bit + 1;
                        emit(
                            RootTid::new(block, offset, root.layout)
                                .map_err(|_| Error::InvalidState)?,
                        )?;
                        emitted = emitted
                            .checked_add(1)
                            .ok_or(Error::Limit("scan candidates"))?;
                    }
                }
                Ok(())
            })?;
        }
        for position in positions.iter_mut().take(groups.len()) {
            *position += 1;
        }
    }
    Ok(emitted)
}
