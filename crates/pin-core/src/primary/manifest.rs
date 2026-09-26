//! bounded two-level sparse index over immutable catalogue pages.

use crate::codec::bytes::{Reader, Writer};
use crate::error::{Error, Result};
use crate::identity::{Generation, HeapLayout, SegmentId};
use crate::mutable::PageStore;
use crate::mutable::page::{NO_BLOCK, PRIMARY_PAYLOAD_BYTES};
use crate::primary::{CatalogueFence, CataloguePage, PrimaryRoot};

const ROOT_MAGIC: &[u8; 4] = b"PMR2";
const LEAF_MAGIC: &[u8; 4] = b"PML2";
const VERSION: u16 = 1;
const ROOT_HEADER: usize = 36;
const LEAF_HEADER: usize = 36;
const ROOT_ENTRY: usize = 16;
const LEAF_ENTRY: usize = 16;

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| Error::Allocation)?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

/// one physical catalogue fence stored in a manifest leaf.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestFence {
    pub segment: SegmentId,
    pub fence: CatalogueFence,
}

/// one finished secondary manifest page and the inclusive range it indexes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedManifestLeaf {
    pub block: u32,
    pub segment: SegmentId,
    pub payload: Vec<u8>,
    pub first_lexeme: Vec<u8>,
    pub last_lexeme: Vec<u8>,
}

/// immutable manifest payload plus bounded leaves. The root payload is block-addressable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedManifest {
    pub root_block: u32,
    pub root_payload: Vec<u8>,
    pub leaves: Vec<EncodedManifestLeaf>,
    pub segment_count: u32,
}

/// encodes one leaf after its physical block has been allocated. This permits
/// the host to commit each leaf before extending the relation again.
pub fn encode_manifest_leaf(
    relation: Generation,
    layout: HeapLayout,
    epoch: u64,
    segment: SegmentId,
    block: u32,
    fences: &[CatalogueFence],
) -> Result<EncodedManifestLeaf> {
    if epoch == 0 || block == 0 || block == NO_BLOCK || fences.is_empty() {
        return Err(Error::InvalidParameters);
    }
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(fences.len())
        .map_err(|_| Error::Allocation)?;
    for fence in fences {
        owned.push(ManifestFence {
            segment,
            fence: CatalogueFence {
                first_lexeme: copy_bytes(&fence.first_lexeme)?,
                last_lexeme: copy_bytes(&fence.last_lexeme)?,
                first_ordinal: fence.first_ordinal,
                block: fence.block,
            },
        });
    }
    validate_fences(segment, &owned)?;
    let payload = encode_leaf(relation, layout, epoch, segment, &owned)?;
    Ok(EncodedManifestLeaf {
        block,
        segment,
        payload,
        first_lexeme: owned
            .first()
            .ok_or(Error::InvalidState)?
            .fence
            .first_lexeme
            .clone(),
        last_lexeme: owned
            .last()
            .ok_or(Error::InvalidState)?
            .fence
            .last_lexeme
            .clone(),
    })
}

fn validate_fences(segment: SegmentId, fences: &[ManifestFence]) -> Result<()> {
    let mut previous: Option<&ManifestFence> = None;
    let mut prior_ordinal = 0;
    for item in fences {
        let f = &item.fence;
        if item.segment != segment
            || f.first_lexeme.is_empty()
            || f.first_lexeme.len() > crate::mutable::document::MAX_TERM_BYTES
            || f.last_lexeme.len() > crate::mutable::document::MAX_TERM_BYTES
            || f.first_lexeme > f.last_lexeme
            || f.block == 0
            || f.block == NO_BLOCK
            || f.first_ordinal == 0
            || previous.is_some_and(|p| {
                p.fence.last_lexeme > f.first_lexeme
                    || p.fence.first_ordinal > f.first_ordinal
                    || p.fence.block == f.block
            })
            || (prior_ordinal != 0 && f.first_ordinal < prior_ordinal)
        {
            return Err(Error::InvalidParameters);
        }
        prior_ordinal = f.first_ordinal;
        previous = Some(item);
    }
    Ok(())
}

/// collects the small root directory while leaf pages are written and committed
/// incrementally. `finish` runs only after the root block is allocated.
pub struct ManifestRootBuilder {
    relation: Generation,
    layout: HeapLayout,
    epoch: u64,
    leaves: Vec<EncodedManifestLeaf>,
    segment_count: u32,
}

impl ManifestRootBuilder {
    pub fn new(relation: Generation, layout: HeapLayout, epoch: u64) -> Result<Self> {
        if epoch == 0 {
            return Err(Error::InvalidParameters);
        }
        Ok(Self {
            relation,
            layout,
            epoch,
            leaves: Vec::new(),
            segment_count: 0,
        })
    }

    pub fn push_leaf(&mut self, leaf: EncodedManifestLeaf) -> Result<()> {
        if leaf.block == 0
            || leaf.block == NO_BLOCK
            || leaf.first_lexeme.is_empty()
            || leaf.first_lexeme > leaf.last_lexeme
            || self.leaves.iter().any(|prior| prior.block == leaf.block)
        {
            return Err(Error::InvalidParameters);
        }
        let decoded = ManifestLeaf::open(
            &leaf.payload,
            self.relation,
            self.layout,
            self.epoch,
            leaf.segment,
        )?;
        if decoded.first()? != leaf.first_lexeme.as_slice()
            || decoded.last()? != leaf.last_lexeme.as_slice()
        {
            return Err(Error::InvalidParameters);
        }
        let mut segment_count = self.segment_count;
        if let Some(prior) = self.leaves.last() {
            if prior.segment > leaf.segment
                || (prior.segment == leaf.segment && prior.last_lexeme > leaf.first_lexeme)
            {
                return Err(Error::InvalidParameters);
            }
            if prior.segment != leaf.segment {
                segment_count = segment_count.checked_add(1).ok_or(Error::InvalidState)?;
            }
        } else {
            segment_count = 1;
        }
        let root_size = self
            .leaves
            .iter()
            .try_fold(ROOT_HEADER, |n, prior| {
                n.checked_add(ROOT_ENTRY)?
                    .checked_add(prior.first_lexeme.len())?
                    .checked_add(prior.last_lexeme.len())
            })
            .and_then(|n| {
                n.checked_add(ROOT_ENTRY)?
                    .checked_add(leaf.first_lexeme.len())?
                    .checked_add(leaf.last_lexeme.len())
            })
            .ok_or(Error::InvalidState)?;
        if root_size > PRIMARY_PAYLOAD_BYTES || self.leaves.len() >= usize::from(u16::MAX) {
            return Err(Error::Limit("manifest root capacity"));
        }
        self.leaves.try_reserve(1).map_err(|_| Error::Allocation)?;
        self.leaves.push(leaf);
        self.segment_count = segment_count;
        Ok(())
    }

    pub fn finish(self, root_block: u32) -> Result<EncodedManifest> {
        if root_block == 0
            || root_block == NO_BLOCK
            || self.leaves.iter().any(|l| l.block == root_block)
        {
            return Err(Error::InvalidParameters);
        }
        let root_payload = encode_root(
            self.relation,
            self.layout,
            self.epoch,
            self.segment_count,
            &self.leaves,
        )?;
        Ok(EncodedManifest {
            root_block,
            root_payload,
            leaves: self.leaves,
            segment_count: self.segment_count,
        })
    }
}

/// bulk helper for small manifests. Streaming writers should use
/// `encode_manifest_leaf` and `ManifestRootBuilder` so pages can be extended
/// and committed one at a time.
pub fn encode_manifest(
    relation: Generation,
    layout: HeapLayout,
    epoch: u64,
    root_block: u32,
    leaf_blocks: &[u32],
    fences: &[ManifestFence],
) -> Result<EncodedManifest> {
    if epoch == 0 || root_block == 0 || root_block == NO_BLOCK || fences.is_empty() {
        return Err(Error::InvalidParameters);
    }
    let mut leaves = Vec::new();
    leaves
        .try_reserve(fences.len().min(PRIMARY_PAYLOAD_BYTES / LEAF_ENTRY))
        .map_err(|_| Error::Allocation)?;
    let mut pos = 0usize;
    let mut previous: Option<&ManifestFence> = None;
    let mut segment_count = 0u32;
    let mut prior_segment = None;
    let mut prior_lexeme: Option<&[u8]> = None;
    let mut ordinal = 0u64;
    while pos < fences.len() {
        let segment = fences[pos].segment;
        if prior_segment.is_some_and(|prior| segment <= prior) && prior_segment != Some(segment) {
            return Err(Error::InvalidParameters);
        }
        if prior_segment != Some(segment) {
            if prior_segment.is_some() && segment <= prior_segment.unwrap() {
                return Err(Error::InvalidParameters);
            }
            segment_count = segment_count.checked_add(1).ok_or(Error::InvalidState)?;
            prior_segment = Some(segment);
            prior_lexeme = None;
            ordinal = 0;
        }
        let leaf_block = *leaf_blocks
            .get(leaves.len())
            .ok_or(Error::InvalidParameters)?;
        if leaf_block == 0 || leaf_block == NO_BLOCK || leaf_block == root_block {
            return Err(Error::InvalidParameters);
        }
        let start = pos;
        let mut size = LEAF_HEADER;
        while pos < fences.len() && fences[pos].segment == segment {
            let item = &fences[pos];
            let fence = &item.fence;
            if item.segment != segment
                || fence.first_lexeme.is_empty()
                || fence.first_lexeme.len() > crate::mutable::document::MAX_TERM_BYTES
                || fence.last_lexeme.len() > crate::mutable::document::MAX_TERM_BYTES
                || fence.first_lexeme > fence.last_lexeme
                || fence.block == 0
                || fence.block == NO_BLOCK
                || fence.first_ordinal == 0
                || (prior_lexeme.is_some_and(|p| p > fence.first_lexeme.as_slice()))
                || (ordinal != 0 && fence.first_ordinal < ordinal)
            {
                return Err(Error::InvalidParameters);
            }
            if previous.is_some_and(|p| p.segment == segment && p.fence.block == fence.block) {
                return Err(Error::InvalidParameters);
            }
            let row_size = LEAF_ENTRY
                .checked_add(fence.first_lexeme.len())
                .and_then(|n| n.checked_add(fence.last_lexeme.len()))
                .ok_or(Error::InvalidState)?;
            if pos > start
                && size
                    .checked_add(row_size)
                    .is_none_or(|n| n > PRIMARY_PAYLOAD_BYTES)
            {
                break;
            }
            if size
                .checked_add(row_size)
                .is_none_or(|n| n > PRIMARY_PAYLOAD_BYTES)
            {
                return Err(Error::Limit("catalogue fence exceeds one manifest leaf"));
            }
            size += row_size;
            prior_lexeme = Some(&fence.last_lexeme);
            ordinal = fence.first_ordinal;
            previous = Some(item);
            pos += 1;
        }
        let items = &fences[start..pos];
        let first = items.first().ok_or(Error::InvalidState)?;
        let last = items.last().ok_or(Error::InvalidState)?;
        let payload = encode_leaf(relation, layout, epoch, segment, items)?;
        leaves.push(EncodedManifestLeaf {
            block: leaf_block,
            segment,
            payload,
            first_lexeme: first.fence.first_lexeme.clone(),
            last_lexeme: last.fence.last_lexeme.clone(),
        });
    }
    if leaves.len() != leaf_blocks.len() {
        return Err(Error::InvalidParameters);
    }
    let root_payload = encode_root(relation, layout, epoch, segment_count, &leaves)?;
    Ok(EncodedManifest {
        root_block,
        root_payload,
        leaves,
        segment_count,
    })
}

fn encode_leaf(
    relation: Generation,
    layout: HeapLayout,
    epoch: u64,
    segment: SegmentId,
    fences: &[ManifestFence],
) -> Result<Vec<u8>> {
    let size = fences
        .iter()
        .try_fold(LEAF_HEADER, |n, f| {
            n.checked_add(LEAF_ENTRY)?
                .checked_add(f.fence.first_lexeme.len())?
                .checked_add(f.fence.last_lexeme.len())
        })
        .ok_or(Error::InvalidState)?;
    if size > PRIMARY_PAYLOAD_BYTES || fences.is_empty() {
        return Err(Error::Limit("manifest leaf size"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(size, 0);
    let mut w = Writer::new(&mut bytes);
    w.put(LEAF_MAGIC)?;
    w.u16(VERSION)?;
    w.u16(u16::try_from(fences.len()).map_err(|_| Error::Limit("manifest leaf entries"))?)?;
    w.u64(relation.get())?;
    w.u16(layout.max_offset())?;
    w.u16(0)?;
    w.u64(epoch)?;
    w.u64(segment.get())?;
    for item in fences {
        let f = &item.fence;
        w.u32(f.block)?;
        w.u64(f.first_ordinal)?;
        w.u16(u16::try_from(f.first_lexeme.len()).map_err(|_| Error::InvalidParameters)?)?;
        w.u16(u16::try_from(f.last_lexeme.len()).map_err(|_| Error::InvalidParameters)?)?;
        w.put(&f.first_lexeme)?;
        w.put(&f.last_lexeme)?;
    }
    Ok(bytes)
}

fn encode_root(
    relation: Generation,
    layout: HeapLayout,
    epoch: u64,
    segment_count: u32,
    leaves: &[EncodedManifestLeaf],
) -> Result<Vec<u8>> {
    let size = leaves
        .iter()
        .try_fold(ROOT_HEADER, |n, leaf| {
            n.checked_add(ROOT_ENTRY)?
                .checked_add(leaf.first_lexeme.len())?
                .checked_add(leaf.last_lexeme.len())
        })
        .ok_or(Error::InvalidState)?;
    if size > PRIMARY_PAYLOAD_BYTES || leaves.is_empty() {
        return Err(Error::Limit("manifest root capacity"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(size, 0);
    let mut w = Writer::new(&mut bytes);
    w.put(ROOT_MAGIC)?;
    w.u16(VERSION)?;
    w.u16(u16::try_from(leaves.len()).map_err(|_| Error::Limit("manifest leaf count"))?)?;
    w.u64(relation.get())?;
    w.u16(layout.max_offset())?;
    w.u16(0)?;
    w.u64(epoch)?;
    w.u32(segment_count)?;
    w.u32(0)?;
    for leaf in leaves {
        w.u64(leaf.segment.get())?;
        w.u32(leaf.block)?;
        w.u16(u16::try_from(leaf.first_lexeme.len()).map_err(|_| Error::InvalidState)?)?;
        w.u16(u16::try_from(leaf.last_lexeme.len()).map_err(|_| Error::InvalidState)?)?;
        w.put(&leaf.first_lexeme)?;
        w.put(&leaf.last_lexeme)?;
    }
    Ok(bytes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LeafDescriptor {
    segment: SegmentId,
    block: u32,
    first: Vec<u8>,
    last: Vec<u8>,
}

/// checked view over the root page's sparse ranges.
pub struct Manifest {
    payload_len: usize,
    relation: Generation,
    layout: HeapLayout,
    epoch: u64,
    segment_count: u32,
    leaves: Vec<LeafDescriptor>,
}

impl Manifest {
    pub fn open(payload: &[u8], expected: PrimaryRoot, root_block: u32) -> Result<Self> {
        if root_block != expected.manifest_block
            || payload.len() < ROOT_HEADER
            || payload.len() > PRIMARY_PAYLOAD_BYTES
        {
            return Err(Error::InvalidState);
        }
        let mut r = Reader::new(payload);
        if r.take(4)? != ROOT_MAGIC || r.u16()? != VERSION {
            return Err(Error::InvalidState);
        }
        let count = usize::from(r.u16()?);
        let relation = Generation::new(r.u64()?).map_err(|_| Error::InvalidState)?;
        let layout = HeapLayout::new(r.u16()?).map_err(|_| Error::InvalidState)?;
        if r.u16()? != 0 {
            return Err(Error::InvalidState);
        }
        let epoch = r.u64()?;
        let segment_count = r.u32()?;
        if r.u32()? != 0
            || relation != expected.relation
            || layout != expected.layout
            || epoch != expected.epoch
            || segment_count != expected.segment_count
            || count == 0
        {
            return Err(Error::InvalidState);
        }
        let mut leaves = Vec::new();
        if count > payload.len().saturating_sub(ROOT_HEADER) / ROOT_ENTRY {
            return Err(Error::InvalidState);
        }
        leaves
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        for _ in 0..count {
            let segment = SegmentId::new(r.u64()?).map_err(|_| Error::InvalidState)?;
            let block = r.u32()?;
            let first_len = usize::from(r.u16()?);
            let last_len = usize::from(r.u16()?);
            let first = copy_bytes(r.take(first_len)?)?;
            let last = copy_bytes(r.take(last_len)?)?;
            if block == 0
                || block == NO_BLOCK
                || first.is_empty()
                || first > last
                || first_len > crate::mutable::document::MAX_TERM_BYTES
                || last_len > crate::mutable::document::MAX_TERM_BYTES
            {
                return Err(Error::InvalidState);
            }
            if leaves
                .iter()
                .any(|prior: &LeafDescriptor| prior.block == block)
            {
                return Err(Error::InvalidState);
            }
            if leaves.last().is_some_and(|p: &LeafDescriptor| {
                p.segment > segment || (p.segment == segment && p.last > first)
            }) {
                return Err(Error::InvalidState);
            }
            leaves.push(LeafDescriptor {
                segment,
                block,
                first,
                last,
            });
        }
        r.finish()?;
        let distinct = leaves
            .iter()
            .enumerate()
            .filter(|(i, leaf)| *i == 0 || leaves[*i - 1].segment != leaf.segment)
            .count();
        if u32::try_from(distinct).ok() != Some(segment_count) {
            return Err(Error::InvalidState);
        }
        Ok(Self {
            payload_len: payload.len(),
            relation,
            layout,
            epoch,
            segment_count,
            leaves,
        })
    }

    pub fn segment_count(&self) -> u32 {
        self.segment_count
    }
    pub fn relation(&self) -> Generation {
        self.relation
    }
    pub fn layout(&self) -> HeapLayout {
        self.layout
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn payload_len(&self) -> usize {
        self.payload_len
    }

    /// Reads only leaf pages whose inclusive lexeme fences can overlap the query.
    pub fn locate<S: PageStore>(
        &self,
        store: &mut S,
        segment: SegmentId,
        key: &[u8],
        prefix: bool,
    ) -> Result<Vec<u32>> {
        if key.is_empty() || key.len() > crate::mutable::document::MAX_TERM_BYTES {
            return Err(Error::InvalidParameters);
        }
        let mut blocks = Vec::new();
        let start = self.leaves.partition_point(|d| {
            d.segment < segment || (d.segment == segment && d.last.as_slice() < key)
        });
        for desc in self.leaves.iter().skip(start) {
            if desc.segment != segment
                || (if prefix {
                    first_is_after_prefix_range(&desc.first, key)
                } else {
                    desc.first.as_slice() > key
                })
            {
                break;
            }
            if !overlaps(&desc.first, &desc.last, key, prefix) {
                continue;
            }
            store.interrupt()?;
            let page = store.read(desc.block)?;
            page.validate(self.layout)?;
            if page.block() != desc.block {
                return Err(Error::InvalidState);
            }
            let payload = page.primary_payload()?;
            let leaf =
                ManifestLeaf::open(payload, self.relation, self.layout, self.epoch, segment)?;
            if leaf.first()? != desc.first || leaf.last()? != desc.last {
                return Err(Error::InvalidState);
            }
            blocks
                .try_reserve(leaf.len())
                .map_err(|_| Error::Allocation)?;
            let fence_start = leaf
                .fences
                .partition_point(|fence| fence.last_lexeme.as_slice() < key);
            for fence in leaf.fences.iter().skip(fence_start) {
                let after_query = if prefix {
                    first_is_after_prefix_range(&fence.first_lexeme, key)
                } else {
                    fence.first_lexeme.as_slice() > key
                };
                if after_query {
                    break;
                }
                if overlaps(&fence.first_lexeme, &fence.last_lexeme, key, prefix) {
                    let catalogue_page = store.read(fence.block)?;
                    catalogue_page.validate(self.layout)?;
                    if catalogue_page.block() != fence.block {
                        return Err(Error::InvalidState);
                    }
                    let catalogue = CataloguePage::open(catalogue_page.primary_payload()?)?;
                    if catalogue.block() != fence.block || catalogue.is_empty() {
                        return Err(Error::InvalidState);
                    }
                    let mut scratch = [0u8; crate::mutable::document::MAX_TERM_BYTES];
                    let (first, first_entry) = catalogue.entry(0, &mut scratch)?;
                    if first != fence.first_lexeme
                        || first_entry.term_ordinal != fence.first_ordinal
                    {
                        return Err(Error::InvalidState);
                    }
                    let (last, _) = catalogue.entry(
                        u16::try_from(catalogue.len() - 1).map_err(|_| Error::InvalidState)?,
                        &mut scratch,
                    )?;
                    if last != fence.last_lexeme {
                        return Err(Error::InvalidState);
                    }
                    blocks.push(fence.block);
                }
            }
        }
        Ok(blocks)
    }
}

fn overlaps(first: &[u8], last: &[u8], key: &[u8], prefix: bool) -> bool {
    if !prefix {
        return first <= key && key <= last;
    }
    last >= key && !first_is_after_prefix_range(first, key)
}

fn first_is_after_prefix_range(first: &[u8], prefix: &[u8]) -> bool {
    for (candidate, byte) in first.iter().zip(prefix) {
        if candidate != byte {
            return candidate > byte;
        }
    }
    false
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestPageFence {
    pub block: u32,
    pub first_ordinal: u64,
    pub first_lexeme: Vec<u8>,
    pub last_lexeme: Vec<u8>,
}

pub struct ManifestLeaf {
    segment: SegmentId,
    fences: Vec<ManifestPageFence>,
}

impl ManifestLeaf {
    pub fn open(
        payload: &[u8],
        relation: Generation,
        layout: HeapLayout,
        epoch: u64,
        segment: SegmentId,
    ) -> Result<Self> {
        if payload.len() < LEAF_HEADER || payload.len() > PRIMARY_PAYLOAD_BYTES {
            return Err(Error::InvalidState);
        }
        let mut r = Reader::new(payload);
        if r.take(4)? != LEAF_MAGIC || r.u16()? != VERSION {
            return Err(Error::InvalidState);
        }
        let count = usize::from(r.u16()?);
        let found_relation = Generation::new(r.u64()?).map_err(|_| Error::InvalidState)?;
        let found_layout = HeapLayout::new(r.u16()?).map_err(|_| Error::InvalidState)?;
        if r.u16()? != 0
            || r.u64()? != epoch
            || SegmentId::new(r.u64()?).map_err(|_| Error::InvalidState)? != segment
            || found_relation != relation
            || found_layout != layout
            || count == 0
        {
            return Err(Error::InvalidState);
        }
        if count > payload.len().saturating_sub(LEAF_HEADER) / LEAF_ENTRY {
            return Err(Error::InvalidState);
        }
        let mut fences = Vec::new();
        fences
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        let mut previous_ordinal = 0;
        for _ in 0..count {
            let block = r.u32()?;
            let first_ordinal = r.u64()?;
            let first_len = usize::from(r.u16()?);
            let last_len = usize::from(r.u16()?);
            let first_lexeme = copy_bytes(r.take(first_len)?)?;
            let last_lexeme = copy_bytes(r.take(last_len)?)?;
            if block == 0
                || block == NO_BLOCK
                || first_ordinal == 0
                || first_lexeme.is_empty()
                || first_lexeme > last_lexeme
                || first_len > crate::mutable::document::MAX_TERM_BYTES
                || last_len > crate::mutable::document::MAX_TERM_BYTES
                || (previous_ordinal != 0 && first_ordinal < previous_ordinal)
            {
                return Err(Error::InvalidState);
            }
            if fences.last().is_some_and(|p: &ManifestPageFence| {
                p.first_lexeme > first_lexeme || p.last_lexeme > first_lexeme || (p.block == block)
            }) {
                return Err(Error::InvalidState);
            }
            fences.push(ManifestPageFence {
                block,
                first_ordinal,
                first_lexeme,
                last_lexeme,
            });
            previous_ordinal = first_ordinal;
        }
        r.finish()?;
        Ok(Self { segment, fences })
    }

    pub fn len(&self) -> usize {
        self.fences.len()
    }
    pub fn is_empty(&self) -> bool {
        self.fences.is_empty()
    }
    pub fn segment(&self) -> SegmentId {
        self.segment
    }
    fn first(&self) -> Result<&[u8]> {
        self.fences
            .first()
            .map(|f| f.first_lexeme.as_slice())
            .ok_or(Error::InvalidState)
    }
    fn last(&self) -> Result<&[u8]> {
        self.fences
            .last()
            .map(|f| f.last_lexeme.as_slice())
            .ok_or(Error::InvalidState)
    }
    pub fn fences(&self) -> &[ManifestPageFence] {
        &self.fences
    }
}

/// loads and validates the root manifest page named by a decoded primary root.
pub fn read_manifest<S: PageStore>(store: &mut S, root: PrimaryRoot) -> Result<Manifest> {
    let page = store.read(root.manifest_block)?;
    page.validate(root.layout)?;
    if page.block() != root.manifest_block {
        return Err(Error::InvalidState);
    }
    Manifest::open(page.primary_payload()?, root, root.manifest_block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutable::page::Page;
    use std::collections::BTreeMap;

    fn layout() -> HeapLayout {
        HeapLayout::new(255).unwrap()
    }
    fn relation_generation() -> Generation {
        Generation::new(7).unwrap()
    }
    fn segment() -> SegmentId {
        SegmentId::new(4).unwrap()
    }
    fn fence(block: u32, first: &[u8], last: &[u8]) -> ManifestFence {
        ManifestFence {
            segment: segment(),
            fence: CatalogueFence {
                first_lexeme: first.to_vec(),
                last_lexeme: last.to_vec(),
                first_ordinal: 1,
                block,
            },
        }
    }

    #[test]
    fn root_and_leaf_round_trip_and_identity_gates() {
        let fences = [
            fence(20, b"alpha", b"bravo"),
            fence(21, b"bravo", b"charlie"),
        ];
        let encoded =
            encode_manifest(relation_generation(), layout(), 9, 10, &[11], &fences).unwrap();
        assert_eq!(encoded.leaves.len(), 1);
        let root = PrimaryRoot {
            relation: relation_generation(),
            layout: layout(),
            epoch: 9,
            manifest_block: 10,
            segment_count: 1,
        };
        let manifest = Manifest::open(&encoded.root_payload, root, 10).unwrap();
        let leaf = ManifestLeaf::open(
            &encoded.leaves[0].payload,
            relation_generation(),
            layout(),
            9,
            segment(),
        )
        .unwrap();
        assert_eq!(leaf.len(), 2);
        assert_eq!(manifest.segment_count(), 1);
        assert!(Manifest::open(&encoded.root_payload, root, 11).is_err());
        assert!(
            Manifest::open(&encoded.root_payload, PrimaryRoot { epoch: 10, ..root }, 10).is_err()
        );
        assert!(
            ManifestLeaf::open(
                &encoded.leaves[0].payload,
                relation_generation(),
                layout(),
                10,
                segment()
            )
            .is_err()
        );
        let mut corrupt = encoded.root_payload.clone();
        corrupt[0] ^= 1;
        assert!(Manifest::open(&corrupt, root, 10).is_err());
    }

    #[test]
    fn oversized_top_level_index_fails_without_truncation() {
        let mut root = ManifestRootBuilder::new(relation_generation(), layout(), 2).unwrap();
        for i in 1..=5u32 {
            let mut key = vec![b'a' + u8::try_from(i).unwrap(); 900];
            key[0] = u8::try_from(i).unwrap();
            let page_fence = CatalogueFence {
                first_lexeme: key.clone(),
                last_lexeme: key,
                first_ordinal: u64::from(i),
                block: 200 + i,
            };
            let leaf = encode_manifest_leaf(
                relation_generation(),
                layout(),
                2,
                segment(),
                100 + i,
                &[page_fence],
            )
            .unwrap();
            let result = root.push_leaf(leaf);
            if i < 5 {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err(), Error::Limit("manifest root capacity"));
            }
        }
    }

    #[test]
    fn inclusive_fences_cover_repeated_term_page_boundaries() {
        assert!(overlaps(b"bravo", b"bravo", b"bravo", false));
        assert!(overlaps(b"bravo", b"bravo", b"bra", true));
        assert!(!overlaps(b"charlie", b"delta", b"bravo", false));
        assert!(!overlaps(b"charlie", b"delta", b"bravo", true));
    }

    #[test]
    fn leaf_pages_can_be_written_before_allocating_the_root() {
        let first = CatalogueFence {
            first_lexeme: b"shared".to_vec(),
            last_lexeme: b"shared".to_vec(),
            first_ordinal: 3,
            block: 40,
        };
        let second = CatalogueFence {
            first_lexeme: b"shared".to_vec(),
            last_lexeme: b"shared".to_vec(),
            first_ordinal: 3,
            block: 41,
        };
        let mut root = ManifestRootBuilder::new(relation_generation(), layout(), 5).unwrap();
        let first_leaf =
            encode_manifest_leaf(relation_generation(), layout(), 5, segment(), 12, &[first])
                .unwrap();
        root.push_leaf(first_leaf).unwrap();
        let second_leaf =
            encode_manifest_leaf(relation_generation(), layout(), 5, segment(), 27, &[second])
                .unwrap();
        root.push_leaf(second_leaf).unwrap();
        let complete = root.finish(50).unwrap();
        let expected = PrimaryRoot {
            relation: relation_generation(),
            layout: layout(),
            epoch: 5,
            manifest_block: 50,
            segment_count: 1,
        };
        let manifest = Manifest::open(&complete.root_payload, expected, 50).unwrap();
        assert_eq!(manifest.leaves.len(), 2);
        assert_eq!(manifest.leaves[0].last, manifest.leaves[1].first);
        assert_eq!(complete.leaves[0].block, 12);
        assert_eq!(complete.leaves[1].block, 27);
    }

    #[test]
    fn strict_store_accepts_incremental_leaf_then_root_write_order() {
        struct StrictStore {
            layout: HeapLayout,
            next: u32,
            pending: bool,
            pages: BTreeMap<u32, Page>,
        }
        impl PageStore for StrictStore {
            fn layout(&self) -> HeapLayout {
                self.layout
            }
            fn blocks(&mut self) -> Result<u32> {
                Ok(self.next)
            }
            fn read(&mut self, block: u32) -> Result<Page> {
                self.pages.get(&block).cloned().ok_or(Error::InvalidState)
            }
            fn extend(&mut self) -> Result<u32> {
                if self.pending {
                    return Err(Error::InvalidState);
                }
                let block = self.next;
                self.next = self.next.checked_add(1).ok_or(Error::InvalidState)?;
                self.pending = true;
                Ok(block)
            }
            fn commit(&mut self, pages: &[&Page]) -> Result<()> {
                if !self.pending || pages.len() != 1 {
                    return Err(Error::InvalidState);
                }
                self.pages.insert(pages[0].block(), (*pages[0]).clone());
                self.pending = false;
                Ok(())
            }
        }
        let mut store = StrictStore {
            layout: layout(),
            next: 1,
            pending: false,
            pages: BTreeMap::new(),
        };
        let mut directory = ManifestRootBuilder::new(relation_generation(), layout(), 6).unwrap();
        for (catalogue_block, term) in [(50, b"a".as_slice()), (51, b"b".as_slice())] {
            let leaf_block = store.extend().unwrap();
            let catalogue_fence = CatalogueFence {
                first_lexeme: term.to_vec(),
                last_lexeme: term.to_vec(),
                first_ordinal: u64::from(catalogue_block - 49),
                block: catalogue_block,
            };
            let leaf = encode_manifest_leaf(
                relation_generation(),
                layout(),
                6,
                segment(),
                leaf_block,
                &[catalogue_fence],
            )
            .unwrap();
            let page = Page::primary(leaf_block, &leaf.payload).unwrap();
            store.commit(&[&page]).unwrap();
            directory.push_leaf(leaf).unwrap();
        }
        let root_block = store.extend().unwrap();
        let complete = directory.finish(root_block).unwrap();
        let root_page = Page::primary(root_block, &complete.root_payload).unwrap();
        store.commit(&[&root_page]).unwrap();
        let root = PrimaryRoot {
            relation: relation_generation(),
            layout: layout(),
            epoch: 6,
            manifest_block: root_block,
            segment_count: 1,
        };
        let manifest = read_manifest(&mut store, root).unwrap();
        assert_eq!(manifest.segment_count(), 1);
        assert_eq!(store.pages.len(), 3);
    }
}
