//! immutable, restart-compressed lexeme catalogue pages.

use crate::codec::bytes::{Reader, Writer};
use crate::codec::{Error as CodecError, ErrorKind};
use crate::error::{Error, Result};
use crate::mutable::document::MAX_TERM_BYTES;
use crate::mutable::page::{NO_BLOCK, PRIMARY_PAYLOAD_BYTES};

const MAGIC: &[u8; 4] = b"CAT2";
const VERSION: u16 = 1;
const HEADER: usize = 24;
const RESTART_EVERY: u16 = 16;

#[derive(Clone, Copy)]
struct Restart {
    record_offset: u16,
    key_offset: u16,
    key_len: u16,
    row: u16,
}

fn corrupt(at: usize) -> Error {
    CodecError::new(at, ErrorKind::InvalidValue).into()
}

/// Address of one term's group-directory record in a primary page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupAddress {
    pub group_base: u32,
    pub block: u32,
    pub offset: u16,
    pub len: u16,
}

impl GroupAddress {
    fn valid(self) -> bool {
        self.group_base & 255 == 0
            && self.block != 0
            && self.block != NO_BLOCK
            && self.len != 0
            && self.offset >= 16
            && usize::from(self.offset) + usize::from(self.len) <= PRIMARY_PAYLOAD_BYTES + 16
    }
}

/// One decoded catalogue row. The lexeme borrows the caller's scratch buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogueEntry {
    pub term_ordinal: u64,
    pub group: GroupAddress,
}

/// First-key fence for a persisted catalogue page, suitable for a sparse page index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogueFence {
    pub first_lexeme: Vec<u8>,
    pub last_lexeme: Vec<u8>,
    pub first_ordinal: u64,
    pub block: u32,
}

/// Finished bounded payload and its corresponding sparse-index fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedCataloguePage {
    pub payload: Vec<u8>,
    pub fence: CatalogueFence,
}

/// Builds one sorted page at a time; callers persist each returned page immediately.
pub struct CatalogueBuilder {
    bytes: Vec<u8>,
    previous: Vec<u8>,
    last_base: Option<u32>,
    first: Vec<u8>,
    restarts: Vec<(u16, Vec<u8>)>,
    restart_bytes: usize,
    count: u16,
    first_ordinal: u64,
    previous_ordinal: u64,
}

impl CatalogueBuilder {
    pub fn new(first_ordinal: u64) -> Result<Self> {
        if first_ordinal == 0 {
            return Err(Error::InvalidParameters);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(PRIMARY_PAYLOAD_BYTES)
            .map_err(|_| Error::Allocation)?;
        let mut previous = Vec::new();
        previous
            .try_reserve_exact(MAX_TERM_BYTES)
            .map_err(|_| Error::Allocation)?;
        let mut first = Vec::new();
        first
            .try_reserve_exact(MAX_TERM_BYTES)
            .map_err(|_| Error::Allocation)?;
        let mut restarts = Vec::new();
        restarts
            .try_reserve_exact(PRIMARY_PAYLOAD_BYTES / 16)
            .map_err(|_| Error::Allocation)?;
        Ok(Self {
            bytes,
            previous,
            last_base: None,
            first,
            restarts,
            restart_bytes: 0,
            count: 0,
            first_ordinal,
            previous_ordinal: first_ordinal.saturating_sub(1),
        })
    }

    /// Adds the next `(lexeme, group_base)` row. Repeated lexemes share one ordinal.
    /// Returns false without consuming it when the current page cannot fit the row.
    pub fn try_push(
        &mut self,
        lexeme: &[u8],
        term_ordinal: u64,
        group: GroupAddress,
    ) -> Result<bool> {
        if lexeme.is_empty()
            || lexeme.len() > MAX_TERM_BYTES
            || !group.valid()
            || term_ordinal == 0
            || (!self.previous.is_empty()
                && (self.previous.as_slice() > lexeme
                    || (self.previous.as_slice() == lexeme
                        && self.last_base.is_some_and(|base| group.group_base <= base))))
            || (!self.previous.is_empty()
                && self.previous.as_slice() < lexeme
                && term_ordinal != self.previous_ordinal + 1)
            || (!self.previous.is_empty()
                && self.previous.as_slice() == lexeme
                && term_ordinal != self.previous_ordinal)
        {
            return Err(Error::InvalidParameters);
        }
        let restart = self.count.is_multiple_of(RESTART_EVERY);
        let prefix = if restart {
            0
        } else {
            self.previous
                .iter()
                .zip(lexeme)
                .take_while(|(a, b)| a == b)
                .count()
        };
        let suffix = &lexeme[prefix..];
        let record_capacity = suffix.len() + 32;
        let mut record = Vec::new();
        record
            .try_reserve_exact(record_capacity)
            .map_err(|_| Error::Allocation)?;
        record.resize(record_capacity, 0);
        let mut writer = Writer::new(&mut record);
        writer.var_u32(prefix as u32)?;
        writer.var_u32(suffix.len() as u32)?;
        writer.put(suffix)?;
        writer.u64(term_ordinal)?;
        writer.u32(group.group_base)?;
        writer.u32(group.block)?;
        writer.u16(group.offset)?;
        writer.u16(group.len)?;
        let record_len = writer.len();
        record.truncate(record_len);
        let restart_cost = if restart { 4 + lexeme.len() } else { 0 };
        if self.count != 0
            && HEADER + self.bytes.len() + self.restart_bytes + record.len() + restart_cost
                > PRIMARY_PAYLOAD_BYTES
        {
            return Ok(false);
        }
        if HEADER + self.bytes.len() + self.restart_bytes + record.len() + restart_cost
            > PRIMARY_PAYLOAD_BYTES
        {
            return Err(Error::InvalidParameters);
        }
        if self.count == 0 {
            self.first_ordinal = term_ordinal;
        }
        if restart {
            let mut key = Vec::new();
            key.try_reserve_exact(lexeme.len())
                .map_err(|_| Error::Allocation)?;
            key.extend_from_slice(lexeme);
            self.restarts.push((self.bytes.len() as u16, key));
            self.restart_bytes += restart_cost;
        }
        if self.count == 0 {
            self.first.extend_from_slice(lexeme);
        }
        self.bytes.extend_from_slice(&record);
        self.previous.clear();
        self.previous.extend_from_slice(lexeme);
        self.last_base = Some(group.group_base);
        self.previous_ordinal = term_ordinal;
        self.count += 1;
        Ok(true)
    }

    /// Finishes a page using the block reserved by the caller after detecting rollover.
    pub fn finish_page_at(&mut self, block: u32) -> Result<EncodedCataloguePage> {
        if self.count == 0 || block == 0 || block == NO_BLOCK {
            return Err(Error::InvalidState);
        }
        let restarts_len = self.restarts.iter().try_fold(0usize, |n, (_, key)| {
            n.checked_add(4 + key.len()).ok_or(Error::InvalidState)
        })?;
        let total = HEADER
            .checked_add(self.bytes.len())
            .and_then(|n| n.checked_add(restarts_len))
            .ok_or(Error::InvalidState)?;
        if total > PRIMARY_PAYLOAD_BYTES {
            return Err(Error::InvalidState);
        }
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(total)
            .map_err(|_| Error::Allocation)?;
        payload.resize(total, 0);
        let mut w = Writer::new(&mut payload);
        w.put(MAGIC)?;
        w.u16(VERSION)?;
        w.u16(self.count)?;
        w.u64(self.first_ordinal)?;
        w.u16(self.bytes.len() as u16)?;
        w.u16(self.restarts.len() as u16)?;
        w.u32(block)?;
        w.put(&self.bytes)?;
        for (offset, key) in &self.restarts {
            w.u16(*offset)?;
            w.u16(key.len() as u16)?;
            w.put(key)?;
        }
        let page = EncodedCataloguePage {
            payload,
            fence: CatalogueFence {
                first_lexeme: self.first.clone(),
                last_lexeme: self.previous.clone(),
                first_ordinal: self.first_ordinal,
                block,
            },
        };
        self.bytes.clear();
        self.first.clear();
        self.restarts.clear();
        self.restart_bytes = 0;
        self.count = 0;
        self.first_ordinal = self.previous_ordinal;
        Ok(page)
    }
}

/// Borrowed view of a single validated catalogue page.
pub struct CataloguePage<'a> {
    payload: &'a [u8],
    count: u16,
    first_ordinal: u64,
    block: u32,
    data_end: usize,
    restarts: Vec<Restart>,
}

impl<'a> CataloguePage<'a> {
    pub fn open(payload: &'a [u8]) -> Result<Self> {
        if payload.len() < HEADER || payload.len() > PRIMARY_PAYLOAD_BYTES {
            return Err(corrupt(0));
        }
        let mut r = Reader::new(payload);
        if r.take(4)? != MAGIC {
            return Err(CodecError::new(0, ErrorKind::BadMagic).into());
        }
        if r.u16()? != VERSION {
            return Err(CodecError::new(4, ErrorKind::UnsupportedVersion).into());
        }
        let count = r.u16()?;
        let first_ordinal = r.u64()?;
        let data_len = usize::from(r.u16()?);
        let restart_count = usize::from(r.u16()?);
        let block = r.u32()?;
        let data_end = HEADER.checked_add(data_len).ok_or_else(|| corrupt(16))?;
        if count == 0
            || first_ordinal == 0
            || block == 0
            || block == NO_BLOCK
            || restart_count != usize::from(count.div_ceil(RESTART_EVERY))
            || data_end > payload.len()
        {
            return Err(corrupt(6));
        }
        let mut pos = HEADER;
        let mut previous = [0u8; MAX_TERM_BYTES];
        let mut current = [0u8; MAX_TERM_BYTES];
        let mut prev_len = 0usize;
        let mut restarts = Vec::new();
        restarts
            .try_reserve_exact(restart_count)
            .map_err(|_| Error::Allocation)?;
        let mut restart_reader = Reader::new(&payload[data_end..]);
        let mut prior_ordinal = 0u64;
        let mut prior_base = 0u32;
        for ordinal in 0..count {
            let start = pos;
            let (len, entry, next) =
                decode_record(payload, pos, data_end, &previous[..prev_len], &mut current)?;
            let lexeme = &current[..len];
            let ordering = previous[..prev_len].cmp(lexeme);
            if len == 0
                || len > MAX_TERM_BYTES
                || (ordinal != 0 && ordering > std::cmp::Ordering::Equal)
                || (ordinal == 0 && entry.term_ordinal != first_ordinal)
                || (ordinal != 0
                    && (entry.term_ordinal < prior_ordinal
                        || entry.term_ordinal > prior_ordinal.saturating_add(1)
                        || (entry.term_ordinal == prior_ordinal
                            && (ordering != std::cmp::Ordering::Equal
                                || entry.group.group_base <= prior_base))))
            {
                return Err(corrupt(start));
            }
            if ordinal.is_multiple_of(RESTART_EVERY) {
                let key_offset = data_end
                    .checked_add(restart_reader.offset())
                    .and_then(|v| v.checked_add(4))
                    .ok_or_else(|| corrupt(data_end))?;
                let off = restart_reader.u16()?;
                let n = usize::from(restart_reader.u16()?);
                if off != u16::try_from(start - HEADER).map_err(|_| corrupt(start))?
                    || n != len
                    || restart_reader.take(n)? != lexeme
                {
                    return Err(corrupt(data_end));
                }
                restarts.push(Restart {
                    record_offset: off,
                    key_offset: u16::try_from(key_offset).map_err(|_| corrupt(key_offset))?,
                    key_len: n as u16,
                    row: (ordinal / RESTART_EVERY) * RESTART_EVERY,
                });
            }
            previous[..len].copy_from_slice(lexeme);
            prev_len = len;
            prior_ordinal = entry.term_ordinal;
            prior_base = entry.group.group_base;
            pos = next;
        }
        if pos != data_end {
            return Err(corrupt(pos));
        }
        restart_reader.finish()?;
        Ok(Self {
            payload,
            count,
            first_ordinal,
            block,
            data_end,
            restarts,
        })
    }

    pub fn len(&self) -> usize {
        usize::from(self.count)
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn first_ordinal(&self) -> u64 {
        self.first_ordinal
    }

    /// physical block encoded in the catalogue header.
    pub fn block(&self) -> u32 {
        self.block
    }

    /// Finds exact bytewise lexeme and writes it nowhere; decoded term identities are stable.
    pub fn lookup(&self, key: &[u8]) -> Result<Option<CatalogueEntry>> {
        self.lookup_counted(key, &mut || {})
    }

    fn lookup_counted<F>(&self, key: &[u8], mut decoded: F) -> Result<Option<CatalogueEntry>>
    where
        F: FnMut(),
    {
        let restart = self.restarts.partition_point(|item| {
            let start = usize::from(item.key_offset);
            let end = start + usize::from(item.key_len);
            &self.payload[start..end] < key
        });
        let slot = restart.saturating_sub(1);
        let mut index = self.restarts.get(slot).map_or(0, |item| item.row);
        let mut pos = self
            .restarts
            .get(slot)
            .map_or(HEADER, |item| HEADER + usize::from(item.record_offset));
        let mut length = 0usize;
        let mut scratch = [0u8; MAX_TERM_BYTES];
        while index < self.count {
            let prior = scratch;
            let (n, entry, next) = decode_record(
                self.payload,
                pos,
                self.data_end,
                &prior[..length],
                &mut scratch,
            )?;
            decoded();
            length = n;
            pos = next;
            match scratch[..length].cmp(key) {
                std::cmp::Ordering::Equal => return Ok(Some(entry)),
                std::cmp::Ordering::Greater => return Ok(None),
                std::cmp::Ordering::Less => index += 1,
            }
        }
        Ok(None)
    }

    /// Returns all group rows for this term on the page. Adjacent pages can also
    /// contain the term; use first/last page fences to find those pages.
    pub fn lookup_range(&self, key: &[u8]) -> Result<std::ops::Range<u16>> {
        self.find_range(key, false)
    }

    /// Returns the contiguous record-number interval matching a byte prefix.
    pub fn prefix_range(&self, prefix: &[u8]) -> Result<std::ops::Range<u16>> {
        self.find_range(prefix, true)
    }

    fn find_range(&self, key: &[u8], prefix: bool) -> Result<std::ops::Range<u16>> {
        let restart = self.restarts.partition_point(|item| {
            let start = usize::from(item.key_offset);
            let end = start + usize::from(item.key_len);
            &self.payload[start..end] < key
        });
        let slot = restart.saturating_sub(1);
        let mut index = self.restarts.get(slot).map_or(0, |item| item.row);
        let mut pos = self
            .restarts
            .get(slot)
            .map_or(HEADER, |item| HEADER + usize::from(item.record_offset));
        let mut length = 0usize;
        let mut scratch = [0u8; MAX_TERM_BYTES];
        let mut first = None;
        let mut end = index;
        while index < self.count {
            let prior = scratch;
            let (n, _, next) = decode_record(
                self.payload,
                pos,
                self.data_end,
                &prior[..length],
                &mut scratch,
            )?;
            length = n;
            pos = next;
            let term = &scratch[..length];
            let matches = if prefix {
                term.starts_with(key)
            } else {
                term == key
            };
            if matches {
                first.get_or_insert(index);
                end = index + 1;
            } else if first.is_some() || (term > key && !key.starts_with(term)) {
                break;
            }
            index += 1;
        }
        Ok(first.map_or(0..0, |start| start..end))
    }

    pub fn entry<'b>(
        &self,
        index: u16,
        scratch: &'b mut [u8; MAX_TERM_BYTES],
    ) -> Result<(&'b [u8], CatalogueEntry)> {
        if index >= self.count {
            return Err(Error::InvalidParameters);
        }
        let slot = usize::from(index / RESTART_EVERY);
        let restart = self.restarts.get(slot).ok_or(Error::InvalidState)?;
        let mut len = 0usize;
        let mut pos = HEADER + usize::from(restart.record_offset);
        let mut result = None;
        for i in restart.row..=index {
            let prior = *scratch;
            let (n, entry, next) =
                decode_record(self.payload, pos, self.data_end, &prior[..len], scratch)?;
            len = n;
            pos = next;
            if i == index {
                result = Some((entry, len));
            }
        }
        let (entry, len) = result.ok_or(Error::InvalidState)?;
        Ok((&scratch[..len], entry))
    }
}

fn decode_record(
    bytes: &[u8],
    pos: usize,
    end: usize,
    previous: &[u8],
    out: &mut [u8],
) -> Result<(usize, CatalogueEntry, usize)> {
    let mut r = Reader::new(bytes.get(pos..end).ok_or_else(|| corrupt(pos))?);
    let prefix = usize::try_from(r.var_u32()?).map_err(|_| corrupt(pos))?;
    let suffix_len = usize::try_from(r.var_u32()?).map_err(|_| corrupt(pos))?;
    if prefix > previous.len()
        || prefix
            .checked_add(suffix_len)
            .is_none_or(|n| n > MAX_TERM_BYTES)
        || prefix + suffix_len > out.len()
    {
        return Err(corrupt(pos));
    }
    let suffix = r.take(suffix_len)?;
    out[..prefix].copy_from_slice(&previous[..prefix]);
    out[prefix..prefix + suffix_len].copy_from_slice(suffix);
    let term_len = prefix + suffix_len;
    let entry = CatalogueEntry {
        term_ordinal: r.u64()?,
        group: GroupAddress {
            group_base: r.u32()?,
            block: r.u32()?,
            offset: r.u16()?,
            len: r.u16()?,
        },
    };
    if !entry.group.valid() {
        return Err(corrupt(pos));
    }
    Ok((term_len, entry, pos + r.offset()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, Result as CoreResult};
    use crate::identity::HeapLayout;
    use crate::mutable::{PageStore, page::Page};
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct SerialStore {
        next: u32,
        outstanding: Option<u32>,
        pages: BTreeMap<u32, Page>,
    }

    impl PageStore for SerialStore {
        fn layout(&self) -> HeapLayout {
            HeapLayout::new(512).unwrap()
        }
        fn blocks(&mut self) -> CoreResult<u32> {
            Ok(self.next)
        }
        fn read(&mut self, block: u32) -> CoreResult<Page> {
            self.pages.get(&block).cloned().ok_or(Error::InvalidState)
        }
        fn extend(&mut self) -> CoreResult<u32> {
            if self.outstanding.is_some() {
                return Err(Error::InvalidState);
            }
            self.next += 1;
            self.outstanding = Some(self.next);
            Ok(self.next)
        }
        fn commit(&mut self, pages: &[&Page]) -> CoreResult<()> {
            if pages.len() != 1
                || pages[0].block() != self.outstanding.ok_or(Error::InvalidState)?
            {
                return Err(Error::InvalidState);
            }
            self.pages.insert(pages[0].block(), pages[0].clone());
            self.outstanding = None;
            Ok(())
        }
    }

    fn addr(i: u32) -> GroupAddress {
        GroupAddress {
            group_base: i * 256,
            block: i + 1,
            offset: 16,
            len: 32,
        }
    }

    fn build(terms: &[Vec<u8>]) -> Vec<EncodedCataloguePage> {
        let mut result = Vec::new();
        let mut b = CatalogueBuilder::new(1).unwrap();
        let mut block = 7;
        for (i, term) in terms.iter().enumerate() {
            if !b.try_push(term, i as u64 + 1, addr(i as u32)).unwrap() {
                result.push(b.finish_page_at(block).unwrap());
                block += 1;
                assert!(b.try_push(term, i as u64 + 1, addr(i as u32)).unwrap());
            }
        }
        if b.count != 0 {
            result.push(b.finish_page_at(block).unwrap());
        }
        result
    }

    #[test]
    fn round_trip_lookup_and_prefix_range() {
        let terms = [
            b"apple".as_slice(),
            b"application",
            b"apply",
            b"banana",
            b"band",
        ]
        .map(<[u8]>::to_vec);
        let pages = build(&terms);
        assert_eq!(pages.len(), 1);
        let page = CataloguePage::open(&pages[0].payload).unwrap();
        assert_eq!(page.len(), 5);
        assert_eq!(page.lookup(b"apply").unwrap().unwrap().term_ordinal, 3);
        assert_eq!(page.lookup(b"apricot").unwrap(), None);
        assert_eq!(page.prefix_range(b"app").unwrap(), 0..3);
        assert_eq!(page.prefix_range(b"z").unwrap(), 0..0);
        assert_eq!(pages[0].fence.first_lexeme, b"apple");
    }

    #[test]
    fn sorted_unique_and_page_limit_are_enforced() {
        let mut b = CatalogueBuilder::new(1).unwrap();
        assert!(b.try_push(b"same", 1, addr(0)).unwrap());
        assert!(b.try_push(b"same", 1, addr(1)).unwrap());
        let terms: Vec<Vec<u8>> = (0..80)
            .map(|i| format!("term-{i:03}-{}", "x".repeat(120)).into_bytes())
            .collect();
        let pages = build(&terms);
        assert!(pages.len() > 1);
        assert!(
            pages
                .iter()
                .all(|page| page.payload.len() <= PRIMARY_PAYLOAD_BYTES)
        );
        let mut prior = 0;
        for encoded in pages {
            let page = CataloguePage::open(&encoded.payload).unwrap();
            assert_eq!(page.first_ordinal(), prior + 1);
            prior += page.len() as u64;
        }
        assert_eq!(prior, 80);
    }

    #[test]
    fn one_term_can_span_catalogue_pages() {
        let mut builder = CatalogueBuilder::new(1).unwrap();
        let mut pages = Vec::new();
        let mut block = 20;
        for base in 0..700u32 {
            if !builder
                .try_push(
                    b"shared",
                    1,
                    GroupAddress {
                        group_base: base * 256,
                        block: 1000 + base,
                        offset: 16,
                        len: 32,
                    },
                )
                .unwrap()
            {
                pages.push(builder.finish_page_at(block).unwrap());
                block += 1;
                assert!(
                    builder
                        .try_push(
                            b"shared",
                            1,
                            GroupAddress {
                                group_base: base * 256,
                                block: 1000 + base,
                                offset: 16,
                                len: 32,
                            }
                        )
                        .unwrap()
                );
            }
        }
        pages.push(builder.finish_page_at(block).unwrap());
        assert!(pages.len() > 1);
        let mut rows = 0;
        for page in &pages {
            assert_eq!(page.fence.first_lexeme, b"shared");
            assert_eq!(page.fence.last_lexeme, b"shared");
            let opened = CataloguePage::open(&page.payload).unwrap();
            let range = opened.lookup_range(b"shared").unwrap();
            assert_eq!(range, 0..opened.len() as u16);
            rows += usize::from(range.end - range.start);
        }
        assert_eq!(rows, 700);
    }

    #[test]
    fn exact_lookup_decodes_one_record_for_a_repeated_term_run() {
        let mut builder = CatalogueBuilder::new(1).unwrap();
        for i in 0..200u32 {
            assert!(
                builder
                    .try_push(
                        b"shared",
                        1,
                        GroupAddress {
                            group_base: i * 256,
                            block: 1000 + i,
                            offset: 16,
                            len: 32,
                        }
                    )
                    .unwrap()
            );
        }
        let encoded = builder.finish_page_at(7).unwrap();
        let page = CataloguePage::open(&encoded.payload).unwrap();

        // Before direct lookup, lookup_range decoded every matching row, then
        // entry decoded the first row again. The optimized path decodes once.
        let old_decode_work = page.lookup_range(b"shared").unwrap().len() + 1;
        let mut decode_work = 0;
        let result = page
            .lookup_counted(b"shared", &mut || decode_work += 1)
            .unwrap();
        assert!(result.is_some());
        assert_eq!(decode_work, 1);
        assert_eq!(old_decode_work, page.len() + 1);
        assert!(decode_work * 100 < old_decode_work);
    }

    #[test]
    fn deferred_blocks_obey_serial_extend_and_do_not_consume_full_row() {
        let mut store = SerialStore::default();
        let mut builder = CatalogueBuilder::new(1).unwrap();
        let terms: Vec<Vec<u8>> = (0..600)
            .map(|i| format!("term-{i:04}-{}", "x".repeat(120)).into_bytes())
            .collect();
        let mut committed = 0;
        for (i, term) in terms.iter().enumerate() {
            let address = addr(i as u32);
            if !builder.try_push(term, i as u64 + 1, address).unwrap() {
                let block = store.extend().unwrap();
                let encoded = builder.finish_page_at(block).unwrap();
                assert_eq!(encoded.fence.block, block);
                let page = Page::primary(block, &encoded.payload).unwrap();
                store.commit(&[&page]).unwrap();
                committed += 1;
                assert!(builder.try_push(term, i as u64 + 1, address).unwrap());
            }
        }
        let block = store.extend().unwrap();
        let encoded = builder.finish_page_at(block).unwrap();
        let page = Page::primary(block, &encoded.payload).unwrap();
        store.commit(&[&page]).unwrap();
        committed += 1;
        assert!(committed > 1);
        assert_eq!(store.pages.len(), committed);
        assert_eq!(store.outstanding, None);
    }

    #[test]
    fn rejects_corrupt_payload_and_group_address() {
        let pages = build(&[b"one".to_vec()]);
        let mut bad = pages[0].payload.clone();
        bad[0] ^= 1;
        assert!(CataloguePage::open(&bad).is_err());
        let mut bad_restart = pages[0].payload.clone();
        *bad_restart.last_mut().unwrap() ^= 1;
        assert!(CataloguePage::open(&bad_restart).is_err());
        let mut b = CatalogueBuilder::new(1).unwrap();
        assert!(
            b.try_push(
                b"x",
                1,
                GroupAddress {
                    group_base: 0,
                    block: NO_BLOCK,
                    offset: 0,
                    len: 1
                }
            )
            .is_err()
        );
    }
}
