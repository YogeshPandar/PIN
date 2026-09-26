//! Checked, explicit little-endian payloads inside PostgreSQL standard pages.
//! Page images are private copies; this module never borrows shared buffers.
//! Owner slots and dictionary offsets remain stable across posting compaction.
//! Format and publication obligations: docs/g3-storage.md.

#[path = "page_grouped.rs"]
mod grouped;
pub use grouped::{
    CATALOG_ENTRIES, CatalogEntry, GROUP_DATA_BYTES, GROUP_DELTA_SEGMENTS, GROUP_RETIRED_SEGMENTS,
    GroupData, GroupDelta, GroupJournal, GroupPageKind, GroupRetired, GroupSnapshot, GroupState,
    MAX_CATALOG_LEVEL,
};

use super::document::{MAX_DOCUMENT_BYTES, MAX_DOCUMENT_TOKENS, MAX_TERM_BYTES};
use crate::analysis::PROFILE_ID;
use crate::codec::bytes::{Reader, Writer};
use crate::codec::records::Publication;
use crate::codec::{Error as CodecError, ErrorKind};
use crate::error::{Error, Result};
use crate::identity::{HeapLayout, Incarnation, RootTid};

pub const NO_BLOCK: u32 = u32::MAX;
pub const CAPACITY: usize = 8192 - 24;
pub const BUCKETS: usize = 512;
pub const MAX_WAL_PAGES: usize = 3;
const HEADER: usize = 16;
const META_HEADER: usize = 64;
const OWNER_HEADER: usize = 20;
const OWNER_BYTES: usize = 40;
const DICTIONARY_ENTRY: usize = 32;
const POSTING_HEADER: usize = 24;
const DIRECT_HEADER: usize = POSTING_HEADER + 32;
const FRAGMENT_HEADER: usize = 36;
pub const FRAGMENT_BYTES: usize = CAPACITY - FRAGMENT_HEADER;
pub const INLINE_BYTES: usize = CAPACITY - OWNER_HEADER - OWNER_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageKind {
    Zero,
    Meta,
    Owners,
    Dictionary,
    Postings,
    SealedPostings,
    DirectPostings,
    Grouped,
    Fragment,
    Free,
}

fn corrupt(offset: usize) -> Error {
    CodecError::new(offset, ErrorKind::InvalidValue).into()
}

fn block_valid(block: u32) -> bool {
    block != 0 && block != NO_BLOCK
}

fn posting_pair_valid(head: u32, tail: u32) -> bool {
    (head == NO_BLOCK && tail == NO_BLOCK) || (block_valid(head) && block_valid(tail))
}

fn pair_valid(head: u32, tail: u32) -> bool {
    (head == NO_BLOCK && tail == NO_BLOCK)
        || (block_valid(head) && block_valid(tail) && tail >= head)
}

/// One unreachable chain retained until recovery or retirement completes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RewriteJournal {
    pub head: u32,
    pub tail: u32,
    pub phase: RewritePhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RewritePhase {
    Building = 1,
    Retiring = 2,
}

/// An immutable owner slot, qualified by its never-reused incarnation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnerRef {
    pub page: u32,
    pub slot: u16,
    pub incarnation: Incarnation,
}

impl OwnerRef {
    fn read(reader: &mut Reader<'_>) -> Result<Self> {
        let page = reader.u32()?;
        let slot = reader.u16()?;
        let reserved = reader.u16()?;
        let incarnation = Incarnation::new(reader.u64()?).map_err(|_| corrupt(reader.offset()))?;
        if !block_valid(page)
            || usize::from(slot) >= (CAPACITY - OWNER_HEADER) / OWNER_BYTES
            || reserved != 0
        {
            return Err(corrupt(reader.offset()));
        }
        Ok(Self {
            page,
            slot,
            incarnation,
        })
    }

    fn write(self, writer: &mut Writer<'_>) -> Result<()> {
        if !block_valid(self.page)
            || usize::from(self.slot) >= (CAPACITY - OWNER_HEADER) / OWNER_BYTES
        {
            return Err(corrupt(writer.len()));
        }
        writer.u32(self.page)?;
        writer.u16(self.slot)?;
        writer.u16(0)?;
        writer.u64(self.incarnation.get())?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TermRef {
    pub page: u32,
    pub offset: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Owner<'a> {
    pub reference: OwnerRef,
    pub root: RootTid,
    pub publication: Publication,
    pub live: bool,
    pub tokens: u32,
    pub terms: u32,
    pub data_head: u32,
    pub data_bytes: u32,
    pub inline: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct Term<'a> {
    pub reference: TermRef,
    pub term: &'a str,
    pub first: OwnerRef,
    pub head: u32,
    pub tail: u32,
}

/// Explicit publication and liveness changes; removal cannot republish an owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnerChange {
    PayloadReady(u32),
    Publish,
    Remove,
    Abandon,
}

/// One bounded private page payload. Mutations never touch a host page directly.
#[derive(Clone)]
pub struct Page {
    block: u32,
    kind: PageKind,
    len: usize,
    initialize: bool,
    bytes: [u8; CAPACITY],
}

impl Page {
    /// Copies one host page through a nonescaping, exclusive output borrow.
    ///
    /// # Errors
    /// Rejects invalid block identities, lengths, magic, tags and reserved fields.
    /// A zero-length read represents a physically all-zero allocation orphan only.
    #[inline(always)]
    pub fn read_with(block: u32, read: impl FnOnce(&mut [u8]) -> Result<usize>) -> Result<Self> {
        if block == NO_BLOCK {
            return Err(corrupt(8));
        }
        let mut page = Self {
            block,
            kind: PageKind::Zero,
            len: 0,
            initialize: false,
            bytes: [0; CAPACITY],
        };
        page.len = read(&mut page.bytes)?;
        if page.len == 0 {
            return Ok(page);
        }
        if page.len > CAPACITY {
            return Err(corrupt(0));
        }
        let mut reader = Reader::new(page.bytes());
        if reader.take(4)? != b"PIN2" {
            return Err(CodecError::new(0, ErrorKind::BadMagic).into());
        }
        if reader.u16()? != 2 {
            return Err(CodecError::new(4, ErrorKind::UnsupportedVersion).into());
        }
        let kind = match reader.u8()? {
            1 => PageKind::Meta,
            2 => PageKind::Owners,
            3 => PageKind::Dictionary,
            4 => PageKind::Postings,
            5 => PageKind::Fragment,
            6 => PageKind::Free,
            7 => PageKind::SealedPostings,
            9 => PageKind::DirectPostings,
            10 => PageKind::Grouped,
            _ => return Err(CodecError::new(6, ErrorKind::UnknownTag).into()),
        };
        if reader.u8()? != 0 || reader.u32()? != block {
            return Err(corrupt(7));
        }
        reader.u32()?;
        page.kind = kind;
        page.check_next()?;
        Ok(page)
    }

    fn new(block: u32, kind: PageKind) -> Result<Self> {
        if block == NO_BLOCK || (block == 0) != (kind == PageKind::Meta) {
            return Err(corrupt(8));
        }
        let tag = match kind {
            PageKind::Zero => return Err(corrupt(6)),
            PageKind::Meta => 1,
            PageKind::Owners => 2,
            PageKind::Dictionary => 3,
            PageKind::Postings => 4,
            PageKind::SealedPostings => 7,
            PageKind::DirectPostings => 9,
            PageKind::Grouped => 10,
            PageKind::Fragment => 5,
            PageKind::Free => 6,
        };
        let mut page = Self {
            block,
            kind,
            len: HEADER,
            initialize: false,
            bytes: [0; CAPACITY],
        };
        let mut writer = Writer::new(&mut page.bytes);
        writer.put(b"PIN2")?;
        writer.u16(2)?;
        writer.u8(tag)?;
        writer.u8(0)?;
        writer.u32(block)?;
        writer.u32(NO_BLOCK)?;
        Ok(page)
    }

    pub fn metadata(layout: HeapLayout) -> Result<Self> {
        let mut page = Self::new(0, PageKind::Meta)?;
        page.len = META_HEADER + BUCKETS * 8;
        page.put_u32(16, PROFILE_ID)?;
        page.put_u32(20, 8192)?;
        page.put_u16(24, layout.max_offset())?;
        page.put_u16(26, BUCKETS as u16)?;
        page.put_u64(32, 1)?;
        for offset in [40, 44, 48] {
            page.put_u32(offset, NO_BLOCK)?;
        }
        for bucket in 0..BUCKETS {
            page.set_bucket(bucket, NO_BLOCK, NO_BLOCK)?;
        }
        Ok(page)
    }

    pub fn owners(block: u32) -> Result<Self> {
        let mut page = Self::new(block, PageKind::Owners)?;
        page.len = OWNER_HEADER;
        Ok(page)
    }

    pub fn dictionary(block: u32) -> Result<Self> {
        Self::new(block, PageKind::Dictionary)
    }

    pub fn postings(block: u32, term: TermRef) -> Result<Self> {
        if !block_valid(term.page) || usize::from(term.offset) < HEADER {
            return Err(corrupt(16));
        }
        let mut page = Self::new(block, PageKind::Postings)?;
        page.len = POSTING_HEADER;
        page.put_u32(16, term.page)?;
        page.put_u16(20, term.offset)?;
        Ok(page)
    }

    pub fn fragment(
        block: u32,
        owner: OwnerRef,
        next: u32,
        offset: u32,
        payload: &[u8],
    ) -> Result<Self> {
        if payload.is_empty() || payload.len() > FRAGMENT_BYTES {
            return Err(corrupt(FRAGMENT_HEADER));
        }
        let mut page = Self::new(block, PageKind::Fragment)?;
        page.len = FRAGMENT_HEADER + payload.len();
        let mut writer = Writer::new(&mut page.bytes[HEADER..]);
        owner.write(&mut writer)?;
        writer.u32(offset)?;
        writer.put(payload)?;
        page.set_next(next)?;
        Ok(page)
    }

    pub fn free(block: u32, next: u32) -> Result<Self> {
        let mut page = Self::new(block, PageKind::Free)?;
        page.set_next(next)?;
        Ok(page)
    }

    pub fn free_from_zero(block: u32, next: u32) -> Result<Self> {
        let mut page = Self::free(block, next)?;
        page.initialize = true;
        Ok(page)
    }

    pub const fn initializes_storage(&self) -> bool {
        self.initialize
    }

    pub const fn block(&self) -> u32 {
        self.block
    }

    pub const fn kind(&self) -> PageKind {
        self.kind
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub fn next(&self) -> Result<u32> {
        self.u32(12)
    }

    pub fn set_next(&mut self, next: u32) -> Result<()> {
        self.put_u32(12, next)?;
        self.check_next()
    }

    fn check_next(&self) -> Result<()> {
        let next = self.next()?;
        if next == NO_BLOCK {
            return Ok(());
        }
        if !block_valid(next) || next == self.block || self.kind == PageKind::Meta {
            return Err(corrupt(12));
        }
        if matches!(self.kind, PageKind::Owners | PageKind::Dictionary) && next <= self.block {
            return Err(corrupt(12));
        }
        Ok(())
    }

    /// Validates every used byte and local record boundary before traversal.
    pub fn validate(&self, layout: HeapLayout) -> Result<()> {
        if self.kind == PageKind::Zero {
            return Ok(());
        }
        self.check_next()?;
        if (self.block == 0) != (self.kind == PageKind::Meta) {
            return Err(corrupt(8));
        }
        match self.kind {
            PageKind::Meta => {
                if !matches!(
                    self.len,
                    grouped::META_LEGACY_BYTES
                        | grouped::META_GROUPED_BYTES
                        | grouped::META_GROUPED_V3_BYTES
                ) || self.u32(16)? != PROFILE_ID
                    || self.u32(20)? != 8192
                    || self.u16(24)? != layout.max_offset()
                    || usize::from(self.u16(26)?) != BUCKETS
                    || self.u32(28)? != 0
                    || self.u64(32)? == 0
                {
                    return Err(corrupt(16));
                }
                self.owner_chain()?;
                self.free_head()?;
                self.rewrite_journal()?;
                self.grouped_state()?;
                for bucket in 0..BUCKETS {
                    self.bucket(bucket)?;
                }
            }
            PageKind::Grouped => self.validate_grouped(layout)?,
            PageKind::Owners => {
                let count = self.owner_count()?;
                let mut payload = OWNER_HEADER + usize::from(count) * OWNER_BYTES;
                for slot in 0..count {
                    let owner = self.owner(slot, layout)?;
                    if !owner.inline.is_empty() {
                        let offset = OWNER_HEADER + usize::from(slot) * OWNER_BYTES;
                        if usize::from(self.u16(offset + 32)?) != payload {
                            return Err(corrupt(offset + 32));
                        }
                        payload += owner.inline.len();
                    }
                }
                if payload != self.len {
                    return Err(corrupt(payload));
                }
            }
            PageKind::Dictionary => {
                for term in self.terms()? {
                    term?;
                }
            }
            PageKind::Postings | PageKind::SealedPostings | PageKind::DirectPostings => {
                let term = self.posting_term()?;
                if !block_valid(term.page) || usize::from(term.offset) < HEADER {
                    return Err(corrupt(16));
                }
                let mut first = None;
                let mut previous: Option<OwnerRef> = None;
                for reference in self.posting_refs()? {
                    let reference = reference?;
                    if previous.is_some_and(|previous| {
                        (reference.page, reference.slot) <= (previous.page, previous.slot)
                            || reference.incarnation <= previous.incarnation
                    }) {
                        return Err(corrupt(POSTING_HEADER));
                    }
                    first.get_or_insert(reference);
                    previous = Some(reference);
                }
                if self.kind == PageKind::DirectPostings {
                    let (header_first, header_last) = self.direct_endpoints()?;
                    if first != Some(header_first) || previous != Some(header_last) {
                        return Err(corrupt(POSTING_HEADER));
                    }
                    for slot in 0..self.u16(22)? {
                        self.direct_root(slot, layout)?;
                    }
                }
            }
            PageKind::Fragment => {
                let (_, offset, payload) = self.fragment_data()?;
                if payload.is_empty()
                    || (offset as usize)
                        .checked_add(payload.len())
                        .is_none_or(|end| end > MAX_DOCUMENT_BYTES)
                {
                    return Err(corrupt(32));
                }
            }
            PageKind::Free => {
                if self.len != HEADER {
                    return Err(corrupt(HEADER));
                }
            }
            PageKind::Zero => {}
        }
        Ok(())
    }

    pub fn reserve_incarnation(&mut self) -> Result<Incarnation> {
        self.require(PageKind::Meta)?;
        let value = self.u64(32)?;
        let next = value
            .checked_add(1)
            .ok_or(Error::Limit("incarnation space"))?;
        let incarnation = Incarnation::new(value).map_err(|_| corrupt(32))?;
        self.put_u64(32, next)?;
        Ok(incarnation)
    }

    pub fn owner_chain(&self) -> Result<(u32, u32)> {
        self.require(PageKind::Meta)?;
        let pair = (self.u32(40)?, self.u32(44)?);
        if !pair_valid(pair.0, pair.1) {
            return Err(corrupt(40));
        }
        Ok(pair)
    }

    pub fn set_owner_chain(&mut self, head: u32, tail: u32) -> Result<()> {
        self.require(PageKind::Meta)?;
        if !pair_valid(head, tail) {
            return Err(corrupt(40));
        }
        self.put_u32(40, head)?;
        self.put_u32(44, tail)
    }

    pub fn bucket(&self, bucket: usize) -> Result<(u32, u32)> {
        self.require(PageKind::Meta)?;
        if bucket >= BUCKETS {
            return Err(corrupt(META_HEADER));
        }
        let offset = META_HEADER + bucket * 8;
        let pair = (self.u32(offset)?, self.u32(offset + 4)?);
        if !pair_valid(pair.0, pair.1) {
            return Err(corrupt(offset));
        }
        Ok(pair)
    }

    pub fn set_bucket(&mut self, bucket: usize, head: u32, tail: u32) -> Result<()> {
        self.require(PageKind::Meta)?;
        if bucket >= BUCKETS || !pair_valid(head, tail) {
            return Err(corrupt(META_HEADER));
        }
        let offset = META_HEADER + bucket * 8;
        self.put_u32(offset, head)?;
        self.put_u32(offset + 4, tail)
    }

    pub fn free_head(&self) -> Result<u32> {
        self.require(PageKind::Meta)?;
        let value = self.u32(48)?;
        if value != NO_BLOCK && !block_valid(value) {
            return Err(corrupt(48));
        }
        Ok(value)
    }

    pub fn set_free_head(&mut self, block: u32) -> Result<()> {
        self.require(PageKind::Meta)?;
        if block != NO_BLOCK && !block_valid(block) {
            return Err(corrupt(48));
        }
        self.put_u32(48, block)
    }

    pub fn rewrite_journal(&self) -> Result<Option<RewriteJournal>> {
        self.require(PageKind::Meta)?;
        let head = self.u32(52)?;
        let tail = self.u32(56)?;
        let phase = match self.u32(60)? {
            0 if head == 0 && tail == 0 => return Ok(None),
            1 => RewritePhase::Building,
            2 => RewritePhase::Retiring,
            _ => return Err(corrupt(60)),
        };
        if !block_valid(head) || !block_valid(tail) {
            return Err(corrupt(52));
        }
        Ok(Some(RewriteJournal { head, tail, phase }))
    }

    pub fn set_rewrite_journal(&mut self, journal: Option<RewriteJournal>) -> Result<()> {
        self.require(PageKind::Meta)?;
        let (head, tail, phase) = match journal {
            Some(journal) => {
                if !block_valid(journal.head) || !block_valid(journal.tail) {
                    return Err(corrupt(52));
                }
                (journal.head, journal.tail, journal.phase as u32)
            }
            None => (0, 0, 0),
        };
        self.put_u32(52, head)?;
        self.put_u32(56, tail)?;
        self.put_u32(60, phase)
    }

    pub fn owner_count(&self) -> Result<u16> {
        self.require(PageKind::Owners)?;
        let count = self.u16(16)?;
        if self.u16(18)? != 0 || OWNER_HEADER + usize::from(count) * OWNER_BYTES > self.len {
            return Err(corrupt(16));
        }
        Ok(count)
    }

    pub fn owner(&self, slot: u16, layout: HeapLayout) -> Result<Owner<'_>> {
        if slot >= self.owner_count()? {
            return Err(corrupt(16));
        }
        let offset = OWNER_HEADER + usize::from(slot) * OWNER_BYTES;
        let mut reader = Reader::new(&self.bytes[offset..offset + OWNER_BYTES]);
        let incarnation = Incarnation::new(reader.u64()?).map_err(|_| corrupt(offset))?;
        let root =
            RootTid::new(reader.u32()?, reader.u16()?, layout).map_err(|_| corrupt(offset + 8))?;
        let publication = match reader.u8()? {
            0 => Publication::Allocated,
            1 => Publication::FragmentsWritten,
            2 => Publication::Published,
            3 => Publication::Abandoned,
            _ => return Err(corrupt(offset + 14)),
        };
        let live = match reader.u8()? {
            0 => false,
            1 if publication == Publication::Published => true,
            _ => return Err(corrupt(offset + 15)),
        };
        let tokens = reader.u32()?;
        let terms = reader.u32()?;
        let data_head = reader.u32()?;
        let data_bytes = reader.u32()?;
        let inline_offset = usize::from(reader.u16()?);
        let inline_len = usize::from(reader.u16()?);
        if reader.u32()? != 0
            || tokens > MAX_DOCUMENT_TOKENS
            || terms > tokens
            || !(16..=MAX_DOCUMENT_BYTES).contains(&(data_bytes as usize))
            || (data_head != NO_BLOCK && !block_valid(data_head))
        {
            return Err(corrupt(offset + 16));
        }
        let inline = if inline_len == 0 {
            if inline_offset != 0 {
                return Err(corrupt(offset + 32));
            }
            &self.bytes[0..0]
        } else {
            if inline_len != data_bytes as usize
                || data_head != NO_BLOCK
                || inline_offset < OWNER_HEADER + usize::from(self.owner_count()?) * OWNER_BYTES
            {
                return Err(corrupt(offset + 32));
            }
            self.bytes()
                .get(inline_offset..inline_offset + inline_len)
                .ok_or_else(|| corrupt(offset + 32))?
        };
        if (publication == Publication::FragmentsWritten || live)
            && inline.is_empty()
            && data_head == NO_BLOCK
        {
            return Err(corrupt(offset + 24));
        }
        if publication == Publication::Allocated && data_head != NO_BLOCK {
            return Err(corrupt(offset + 24));
        }
        Ok(Owner {
            reference: OwnerRef {
                page: self.block,
                slot,
                incarnation,
            },
            root,
            publication,
            live,
            tokens,
            terms,
            data_head,
            data_bytes,
            inline,
        })
    }

    /// Appends a never-reused slot, packing small payloads into the owner page.
    /// Returns None without mutation when this page is full.
    pub fn append_owner(
        &mut self,
        incarnation: Incarnation,
        root: RootTid,
        tokens: u32,
        terms: u32,
        payload: &[u8],
    ) -> Result<Option<OwnerRef>> {
        let count = self.owner_count()?;
        if tokens > MAX_DOCUMENT_TOKENS
            || terms > tokens
            || !(16..=MAX_DOCUMENT_BYTES).contains(&payload.len())
        {
            return Err(Error::InvalidDocument);
        }
        let inline = if payload.len() <= INLINE_BYTES {
            payload
        } else {
            &[]
        };
        let new_len = self.len + OWNER_BYTES + inline.len();
        if new_len > CAPACITY {
            return Ok(None);
        }
        let offset = OWNER_HEADER + usize::from(count) * OWNER_BYTES;
        // fixed slots stay in place; only private inline bytes move past the new slot.
        self.bytes
            .copy_within(offset..self.len, offset + OWNER_BYTES);
        for slot in 0..count {
            let record = OWNER_HEADER + usize::from(slot) * OWNER_BYTES;
            let inline_offset = self.u16(record + 32)?;
            if inline_offset != 0 {
                self.put_u16(
                    record + 32,
                    inline_offset
                        .checked_add(OWNER_BYTES as u16)
                        .ok_or_else(|| corrupt(record + 32))?,
                )?;
            }
        }
        let inline_offset = self.len + OWNER_BYTES;
        self.len = new_len;
        let mut writer = Writer::new(&mut self.bytes[offset..offset + OWNER_BYTES]);
        writer.u64(incarnation.get())?;
        writer.u32(root.block())?;
        writer.u16(root.offset())?;
        writer.u8(0)?;
        writer.u8(0)?;
        writer.u32(tokens)?;
        writer.u32(terms)?;
        writer.u32(NO_BLOCK)?;
        writer.u32(payload.len() as u32)?;
        writer.u16(if inline.is_empty() {
            0
        } else {
            inline_offset as u16
        })?;
        writer.u16(inline.len() as u16)?;
        writer.u32(0)?;
        self.bytes[inline_offset..new_len].copy_from_slice(inline);
        self.put_u16(16, count + 1)?;
        Ok(Some(OwnerRef {
            page: self.block,
            slot: count,
            incarnation,
        }))
    }

    /// Applies one explicit publication or liveness change.
    /// Invalid transitions leave the page unchanged.
    pub fn change_owner(
        &mut self,
        reference: OwnerRef,
        change: OwnerChange,
        layout: HeapLayout,
    ) -> Result<()> {
        let owner = self.owner(reference.slot, layout)?;
        if owner.reference != reference {
            return Err(corrupt(16));
        }
        let (state, live, head) = match change {
            OwnerChange::PayloadReady(head) if owner.publication == Publication::Allocated => {
                if (owner.inline.is_empty() && !block_valid(head))
                    || (!owner.inline.is_empty() && head != NO_BLOCK)
                {
                    return Err(corrupt(24));
                }
                (1, false, head)
            }
            OwnerChange::Publish if owner.publication == Publication::FragmentsWritten => {
                (2, true, owner.data_head)
            }
            OwnerChange::Remove if owner.publication == Publication::Published && owner.live => {
                (2, false, NO_BLOCK)
            }
            OwnerChange::Abandon
                if matches!(
                    owner.publication,
                    Publication::Allocated | Publication::FragmentsWritten
                ) =>
            {
                (3, false, NO_BLOCK)
            }
            _ => return Err(Error::InvalidState),
        };
        let offset = OWNER_HEADER + usize::from(reference.slot) * OWNER_BYTES;
        self.bytes[offset + 14] = state;
        self.bytes[offset + 15] = u8::from(live);
        self.put_u32(offset + 24, head)
    }

    pub fn terms(&self) -> Result<Terms<'_>> {
        self.require(PageKind::Dictionary)?;
        Ok(Terms {
            page: self.block,
            reader: Reader::new(&self.bytes()[HEADER..]),
            failed: false,
        })
    }

    pub fn append_term(&mut self, term: &str, first: OwnerRef) -> Result<Option<TermRef>> {
        self.require(PageKind::Dictionary)?;
        if term.is_empty() || term.len() > MAX_TERM_BYTES {
            return Err(Error::InvalidDocument);
        }
        let next = self.len + DICTIONARY_ENTRY + term.len();
        if next > CAPACITY {
            return Ok(None);
        }
        let reference = TermRef {
            page: self.block,
            offset: self.len as u16,
        };
        let mut writer = Writer::new(&mut self.bytes[self.len..next]);
        writer.u16(term.len() as u16)?;
        writer.u16(0)?;
        writer.u32(NO_BLOCK)?;
        writer.u32(NO_BLOCK)?;
        first.write(&mut writer)?;
        writer.u32(0)?;
        writer.put(term.as_bytes())?;
        self.len = next;
        Ok(Some(reference))
    }

    pub fn term(&self, reference: TermRef) -> Result<Term<'_>> {
        if reference.page != self.block {
            return Err(corrupt(8));
        }
        for term in self.terms()? {
            let term = term?;
            if term.reference == reference {
                return Ok(term);
            }
        }
        Err(corrupt(usize::from(reference.offset)))
    }

    pub fn set_posting_chain(&mut self, term: TermRef, head: u32, tail: u32) -> Result<()> {
        self.term(term)?;
        if !posting_pair_valid(head, tail) {
            return Err(corrupt(usize::from(term.offset)));
        }
        self.put_u32(usize::from(term.offset) + 4, head)?;
        self.put_u32(usize::from(term.offset) + 8, tail)
    }

    pub fn posting_term(&self) -> Result<TermRef> {
        if !matches!(
            self.kind,
            PageKind::Postings | PageKind::SealedPostings | PageKind::DirectPostings
        ) {
            return Err(corrupt(6));
        }
        if self.kind == PageKind::Postings && self.u16(22)? != 0 {
            return Err(corrupt(22));
        }
        Ok(TermRef {
            page: self.u32(16)?,
            offset: self.u16(20)?,
        })
    }

    pub fn posting_refs(&self) -> Result<Postings<'_>> {
        self.posting_term()?;
        let compressed = self.kind != PageKind::Postings;
        let count = if compressed {
            let count = self.u16(22)?;
            if count == 0 || usize::from(count) > (self.len - POSTING_HEADER) / 3 {
                return Err(corrupt(22));
            }
            count
        } else {
            if !(self.len - POSTING_HEADER).is_multiple_of(16) {
                return Err(corrupt(POSTING_HEADER));
            }
            ((self.len - POSTING_HEADER) / 16) as u16
        };
        Ok(Postings {
            reader: Reader::new(
                self.bytes()
                    .get(self.posting_start()?..)
                    .ok_or_else(|| corrupt(22))?,
            ),
            compressed,
            remaining: count,
            previous: None,
            failed: false,
        })
    }

    fn posting_start(&self) -> Result<usize> {
        Ok(if self.kind == PageKind::DirectPostings {
            DIRECT_HEADER
        } else {
            POSTING_HEADER
        } + if self.kind == PageKind::DirectPostings {
            usize::from(self.u16(22)?) * 8
        } else {
            0
        })
    }

    // reads the validated stream count without decoding owner references.
    pub(super) fn posting_records(&self) -> Result<usize> {
        Ok(usize::from(self.posting_refs()?.remaining))
    }

    /// Returns the number of records in a checked direct page.
    pub fn posting_count(&self) -> Result<u16> {
        self.require(PageKind::DirectPostings)?;
        self.u16(22)
    }

    /// Bounds the ordered owner range of one direct page without decoding it.
    pub fn direct_endpoints(&self) -> Result<(OwnerRef, OwnerRef)> {
        self.require(PageKind::DirectPostings)?;
        let first = self
            .bytes()
            .get(POSTING_HEADER..POSTING_HEADER + 16)
            .ok_or_else(|| corrupt(POSTING_HEADER))?;
        let last = self
            .bytes()
            .get(POSTING_HEADER + 16..DIRECT_HEADER)
            .ok_or_else(|| corrupt(POSTING_HEADER + 16))?;
        Ok((
            OwnerRef::read(&mut Reader::new(first))?,
            OwnerRef::read(&mut Reader::new(last))?,
        ))
    }

    /// Reads a local live coordinate; it never certifies snapshot visibility.
    pub fn direct_root(&self, slot: u16, layout: HeapLayout) -> Result<Option<RootTid>> {
        self.require(PageKind::DirectPostings)?;
        if slot >= self.u16(22)? {
            return Err(corrupt(22));
        }
        let offset = DIRECT_HEADER + usize::from(slot) * 8;
        let bytes = self
            .bytes()
            .get(offset..offset + 8)
            .ok_or_else(|| corrupt(offset))?;
        let mut reader = Reader::new(bytes);
        let block = reader.u32()?;
        let offset = reader.u16()?;
        let live = reader.u8()?;
        if live > 1 || reader.u8()? != 0 {
            return Err(corrupt(POSTING_HEADER));
        }
        let root = RootTid::new(block, offset, layout).map_err(|_| corrupt(POSTING_HEADER))?;
        Ok((live == 1).then_some(root))
    }

    /// Clears a copied coordinate before the host can recycle its heap slot.
    pub fn remove_direct_root(&mut self, slot: u16, layout: HeapLayout) -> Result<()> {
        self.direct_root(slot, layout)?;
        self.bytes[DIRECT_HEADER + usize::from(slot) * 8 + 6] = 0;
        Ok(())
    }

    pub fn append_posting(&mut self, owner: OwnerRef) -> Result<bool> {
        self.require(PageKind::Postings)?;
        if self.len + 16 > CAPACITY {
            return Ok(false);
        }
        owner.write(&mut Writer::new(&mut self.bytes[self.len..self.len + 16]))?;
        self.len += 16;
        Ok(true)
    }

    pub fn fragment_data(&self) -> Result<(OwnerRef, u32, &[u8])> {
        self.require(PageKind::Fragment)?;
        let mut reader = Reader::new(&self.bytes()[HEADER..]);
        let owner = OwnerRef::read(&mut reader)?;
        let offset = reader.u32()?;
        Ok((owner, offset, reader.take(reader.remaining())?))
    }

    fn require(&self, kind: PageKind) -> Result<()> {
        if self.kind != kind {
            return Err(corrupt(6));
        }
        Ok(())
    }

    fn u16(&self, offset: usize) -> Result<u16> {
        Ok(Reader::new(self.bytes().get(offset..).ok_or_else(|| corrupt(offset))?).u16()?)
    }

    fn u32(&self, offset: usize) -> Result<u32> {
        Ok(Reader::new(self.bytes().get(offset..).ok_or_else(|| corrupt(offset))?).u32()?)
    }

    fn u64(&self, offset: usize) -> Result<u64> {
        Ok(Reader::new(self.bytes().get(offset..).ok_or_else(|| corrupt(offset))?).u64()?)
    }

    fn put_u16(&mut self, offset: usize, value: u16) -> Result<()> {
        Ok(Writer::new(
            self.bytes
                .get_mut(offset..offset + 2)
                .ok_or_else(|| corrupt(offset))?,
        )
        .u16(value)?)
    }

    fn put_u32(&mut self, offset: usize, value: u32) -> Result<()> {
        Ok(Writer::new(
            self.bytes
                .get_mut(offset..offset + 4)
                .ok_or_else(|| corrupt(offset))?,
        )
        .u32(value)?)
    }

    fn put_u64(&mut self, offset: usize, value: u64) -> Result<()> {
        Ok(Writer::new(
            self.bytes
                .get_mut(offset..offset + 8)
                .ok_or_else(|| corrupt(offset))?,
        )
        .u64(value)?)
    }
}

/// Builds one compressed page without a heap allocation or a decoder rescan.
pub struct SealedBuilder {
    page: Page,
    previous: Option<OwnerRef>,
    count: u16,
}

impl SealedBuilder {
    pub fn new(block: u32, term: TermRef) -> Result<Self> {
        let mut page = Page::postings(block, term)?;
        page.kind = PageKind::SealedPostings;
        page.bytes[6] = 7;
        Ok(Self {
            page,
            previous: None,
            count: 0,
        })
    }

    pub fn new_direct(block: u32, term: TermRef) -> Result<Self> {
        let mut builder = Self::new(block, term)?;
        builder.page.kind = PageKind::DirectPostings;
        builder.page.bytes[6] = 9;
        builder.page.len = DIRECT_HEADER;
        Ok(builder)
    }

    /// Builds parallel fixed coordinate and compressed identity streams.
    pub fn push_direct(&mut self, owner: OwnerRef, root: RootTid) -> Result<bool> {
        self.page.require(PageKind::DirectPostings)?;
        let mut bytes = [0u8; 20];
        let mut writer = Writer::new(&mut bytes);
        write_delta(&mut writer, owner, self.previous)?;
        let len = writer.len();
        if self.page.len + len + 8 > CAPACITY {
            return Ok(false);
        }
        let start = DIRECT_HEADER + usize::from(self.count) * 8;
        self.page.bytes.copy_within(start..self.page.len, start + 8);
        let mut coordinate = Writer::new(&mut self.page.bytes[start..start + 8]);
        coordinate.u32(root.block())?;
        coordinate.u16(root.offset())?;
        coordinate.u8(1)?;
        coordinate.u8(0)?;
        self.page.len += 8;
        self.page.bytes[self.page.len..self.page.len + len].copy_from_slice(&bytes[..len]);
        self.page.len += len;
        if self.count == 0 {
            owner.write(&mut Writer::new(
                &mut self.page.bytes[POSTING_HEADER..POSTING_HEADER + 16],
            ))?;
        }
        owner.write(&mut Writer::new(
            &mut self.page.bytes[POSTING_HEADER + 16..DIRECT_HEADER],
        ))?;
        self.count += 1;
        self.page.put_u16(22, self.count)?;
        self.previous = Some(owner);
        Ok(true)
    }

    /// Returns false without mutation when this page has no room for the owner.
    pub fn push(&mut self, owner: OwnerRef) -> Result<bool> {
        self.page.require(PageKind::SealedPostings)?;
        let mut bytes = [0u8; 20];
        let mut writer = Writer::new(&mut bytes);
        write_delta(&mut writer, owner, self.previous)?;
        let len = writer.len();
        if self.page.len + len > CAPACITY {
            return Ok(false);
        }
        self.page.bytes[self.page.len..self.page.len + len].copy_from_slice(&bytes[..len]);
        self.page.len += len;
        self.count += 1;
        self.page.put_u16(22, self.count)?;
        self.previous = Some(owner);
        Ok(true)
    }

    pub fn finish(self) -> Result<Page> {
        if self.count == 0 {
            return Err(Error::InvalidState);
        }
        Ok(self.page)
    }
}

// page/slot order follows stable owner allocation, not reusable posting blocks.
fn write_delta(writer: &mut Writer<'_>, owner: OwnerRef, previous: Option<OwnerRef>) -> Result<()> {
    let (page, slot, incarnation) = match previous {
        Some(previous) => {
            if owner.page < previous.page
                || (owner.page == previous.page && owner.slot <= previous.slot)
                || owner.incarnation.get() <= previous.incarnation.get()
            {
                return Err(Error::InvalidState);
            }
            (
                owner.page - previous.page,
                if owner.page == previous.page {
                    owner.slot - previous.slot
                } else {
                    owner.slot
                },
                owner.incarnation.get() - previous.incarnation.get(),
            )
        }
        None => (owner.page, owner.slot, owner.incarnation.get()),
    };
    if !block_valid(owner.page)
        || usize::from(owner.slot) >= (CAPACITY - OWNER_HEADER) / OWNER_BYTES
    {
        return Err(Error::InvalidState);
    }
    writer.var_u32(page)?;
    writer.var_u32(u32::from(slot))?;
    writer.var_u64(incarnation)?;
    Ok(())
}

fn read_delta(reader: &mut Reader<'_>, previous: Option<OwnerRef>) -> Result<OwnerRef> {
    let start = reader.offset();
    let page_delta = reader.var_u32()?;
    let slot_delta = reader.var_u32()?;
    let incarnation_delta = reader.var_u64()?;
    let (page, slot, incarnation) = match previous {
        Some(previous) => {
            if incarnation_delta == 0 || (page_delta == 0 && slot_delta == 0) {
                return Err(corrupt(start));
            }
            (
                previous
                    .page
                    .checked_add(page_delta)
                    .ok_or_else(|| corrupt(start))?,
                if page_delta == 0 {
                    u32::from(previous.slot)
                        .checked_add(slot_delta)
                        .ok_or_else(|| corrupt(start))?
                } else {
                    slot_delta
                },
                previous
                    .incarnation
                    .get()
                    .checked_add(incarnation_delta)
                    .ok_or_else(|| corrupt(start))?,
            )
        }
        None => (page_delta, slot_delta, incarnation_delta),
    };
    if !block_valid(page) || slot as usize >= (CAPACITY - OWNER_HEADER) / OWNER_BYTES {
        return Err(corrupt(start));
    }
    Ok(OwnerRef {
        page,
        slot: slot as u16,
        incarnation: Incarnation::new(incarnation).map_err(|_| corrupt(start))?,
    })
}

pub struct Terms<'a> {
    page: u32,
    reader: Reader<'a>,
    failed: bool,
}

impl<'a> Iterator for Terms<'a> {
    type Item = Result<Term<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.reader.remaining() == 0 {
            return None;
        }
        let offset = self.reader.offset() + HEADER;
        let result = (|| {
            let len = usize::from(self.reader.u16()?);
            if len == 0 || len > MAX_TERM_BYTES || self.reader.u16()? != 0 {
                return Err(corrupt(offset));
            }
            let head = self.reader.u32()?;
            let tail = self.reader.u32()?;
            let first = OwnerRef::read(&mut self.reader)?;
            if !posting_pair_valid(head, tail) || self.reader.u32()? != 0 {
                return Err(corrupt(offset + 4));
            }
            let term = std::str::from_utf8(self.reader.take(len)?)
                .map_err(|_| corrupt(offset + DICTIONARY_ENTRY))?;
            Ok(Term {
                reference: TermRef {
                    page: self.page,
                    offset: offset as u16,
                },
                term,
                first,
                head,
                tail,
            })
        })();
        self.failed = result.is_err();
        Some(result)
    }
}

impl std::iter::FusedIterator for Terms<'_> {}

pub struct Postings<'a> {
    reader: Reader<'a>,
    compressed: bool,
    remaining: u16,
    previous: Option<OwnerRef>,
    failed: bool,
}

impl Iterator for Postings<'_> {
    type Item = Result<OwnerRef>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        if self.remaining == 0 {
            if self.reader.remaining() == 0 {
                return None;
            }
            self.failed = true;
            return Some(Err(corrupt(self.reader.offset())));
        }
        let result = if self.compressed {
            read_delta(&mut self.reader, self.previous)
        } else {
            OwnerRef::read(&mut self.reader)
        };
        self.remaining -= 1;
        match result {
            Ok(owner) => self.previous = Some(owner),
            Err(_) => self.failed = true,
        }
        Some(result)
    }
}

impl std::iter::FusedIterator for Postings<'_> {}

// owns encoded bytes and decoder offsets without a self-referential borrow.
pub(super) struct OwnedPostings {
    page: Page,
    offset: usize,
    compressed: bool,
    remaining: u16,
    previous: Option<OwnerRef>,
    failed: bool,
}

impl OwnedPostings {
    pub(super) fn new(page: Page) -> Result<Self> {
        let postings = page.posting_refs()?;
        let (compressed, remaining) = (postings.compressed, postings.remaining);
        let offset = page.posting_start()?;
        Ok(Self {
            page,
            offset,
            compressed,
            remaining,
            previous: None,
            failed: false,
        })
    }

    pub(super) fn page(&self) -> &Page {
        &self.page
    }

    pub(super) fn current_direct(&self, layout: HeapLayout) -> Result<Option<Option<RootTid>>> {
        if self.page.kind != PageKind::DirectPostings {
            return Ok(None);
        }
        let slot = self
            .page
            .u16(22)?
            .checked_sub(self.remaining + 1)
            .ok_or(Error::InvalidState)?;
        Ok(Some(self.page.direct_root(slot, layout)?))
    }
}

impl Iterator for OwnedPostings {
    type Item = Result<OwnerRef>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let Some(bytes) = self.page.bytes().get(self.offset..) else {
            self.failed = true;
            return Some(Err(Error::InvalidState));
        };
        // resume the shared decoder at its exact byte offset, never rescan a prefix.
        let mut postings = Postings {
            reader: Reader::new(bytes),
            compressed: self.compressed,
            remaining: self.remaining,
            previous: self.previous,
            failed: false,
        };
        let value = postings.next();
        self.offset += postings.reader.offset();
        self.remaining = postings.remaining;
        self.previous = postings.previous;
        self.failed = postings.failed;
        value
    }
}

impl std::iter::FusedIterator for OwnedPostings {}

/// Stable bucket routing only; exact byte equality always resolves collisions.
pub fn bucket_for(term: &str) -> usize {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in term.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    (hash as usize) & (BUCKETS - 1)
}
