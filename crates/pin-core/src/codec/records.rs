// versioned document, nullable text and source-manifest payloads.
// relation identity comes from the enclosing source; these records prove no mvcc facts.

use crate::identity::{Generation, HeapLayout, Incarnation, RootTid, SegmentId};
use super::bytes::{Reader, Writer};
use super::{Error, ErrorKind, Result};

pub const HEADER_BYTES: usize = 16;
pub const FORMAT_VERSION: u16 = 1;

pub(crate) fn payload(bytes: &[u8], kind: u8, max_bytes: usize) -> Result<&[u8]> {
    if bytes.len() > max_bytes {
        return Err(Error::new(0, ErrorKind::LimitExceeded));
    }
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != b"PIN1" { return Err(Error::new(0, ErrorKind::BadMagic)); }
    if reader.u8()? != kind { return Err(Error::new(4, ErrorKind::UnknownTag)); }
    if reader.u8()? != 0 { return Err(Error::new(5, ErrorKind::NonCanonical)); }
    if reader.u16()? != FORMAT_VERSION { return Err(Error::new(6, ErrorKind::UnsupportedVersion)); }
    if reader.u32()? != 0 { return Err(Error::new(8, ErrorKind::UnsupportedFeatures)); }
    let len = usize::try_from(reader.u32()?).map_err(|_| Error::new(12, ErrorKind::Overflow))?;
    let result = reader.take(len)?;
    reader.finish()?;
    Ok(result)
}

pub(crate) fn start(output: &mut [u8], kind: u8, len: usize) -> Result<Writer<'_>> {
    let total = len.checked_add(HEADER_BYTES).ok_or(Error::new(0, ErrorKind::Overflow))?;
    let len = u32::try_from(len).map_err(|_| Error::new(0, ErrorKind::Overflow))?;
    if output.len() < total { return Err(Error::new(0, ErrorKind::Truncated)); }
    let mut writer = Writer::new(output);
    writer.put(b"PIN1")?;
    writer.u8(kind)?;
    writer.u8(0)?;
    writer.u16(FORMAT_VERSION)?;
    writer.u32(0)?;
    writer.u32(len)?;
    Ok(writer)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Publication { Allocated, FragmentsWritten, Published, Abandoned }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentRecord {
    pub segment: SegmentId,
    pub incarnation: Incarnation,
    pub root: RootTid,
    pub token_count: u32,
    pub profile: u32,
    pub publication: Publication,
    pub live: bool,
}

impl DocumentRecord {
    pub const ENCODED_BYTES: usize = HEADER_BYTES + 36;

    fn validate(self) -> Result<()> {
        if self.profile == 0 || (self.live && self.publication != Publication::Published) {
            return Err(Error::new(0, ErrorKind::InvalidValue));
        }
        Ok(())
    }

    pub fn encode(self, output: &mut [u8]) -> Result<usize> {
        self.validate()?;
        let mut writer = start(output, 1, 36)?;
        writer.u64(self.segment.get())?;
        writer.u64(self.incarnation.get())?;
        writer.u32(self.root.block())?;
        writer.u16(self.root.offset())?;
        writer.u16(0)?;
        writer.u32(self.token_count)?;
        writer.u32(self.profile)?;
        writer.u8(match self.publication {
            Publication::Allocated => 0, Publication::FragmentsWritten => 1,
            Publication::Published => 2, Publication::Abandoned => 3,
        })?;
        writer.u8(u8::from(self.live))?;
        writer.u16(0)?;
        Ok(writer.len())
    }

    pub fn parse(bytes: &[u8], layout: HeapLayout) -> Result<Self> {
        let mut reader = Reader::new(payload(bytes, 1, Self::ENCODED_BYTES)?);
        let segment = SegmentId::new(reader.u64()?).map_err(|_| Error::new(0, ErrorKind::InvalidValue))?;
        let incarnation = Incarnation::new(reader.u64()?).map_err(|_| Error::new(8, ErrorKind::InvalidValue))?;
        let block = reader.u32()?;
        let offset = reader.u16()?;
        let root = RootTid::new(block, offset, layout).map_err(|_| Error::new(16, ErrorKind::InvalidValue))?;
        if reader.u16()? != 0 { return Err(Error::new(22, ErrorKind::NonCanonical)); }
        let token_count = reader.u32()?;
        let profile = reader.u32()?;
        let publication = match reader.u8()? {
            0 => Publication::Allocated, 1 => Publication::FragmentsWritten,
            2 => Publication::Published, 3 => Publication::Abandoned,
            _ => return Err(Error::new(32, ErrorKind::UnknownTag)),
        };
        let live = match reader.u8()? {
            0 => false, 1 => true, _ => return Err(Error::new(33, ErrorKind::InvalidValue)),
        };
        if reader.u16()? != 0 { return Err(Error::new(34, ErrorKind::NonCanonical)); }
        reader.finish()?;
        let record = Self { segment, incarnation, root, token_count, profile, publication, live };
        record.validate()?;
        Ok(record)
    }
}

// preserves null versus empty text; returned text borrows the validated input.
pub fn decode_value(bytes: &[u8], max_bytes: usize) -> Result<Option<&str>> {
    let mut reader = Reader::new(payload(bytes, 2, max_bytes)?);
    let result = match reader.u8()? {
        0 => None,
        1 => {
            let len = usize::try_from(reader.u32()?).map_err(|_| Error::new(1, ErrorKind::Overflow))?;
            Some(std::str::from_utf8(reader.take(len)?).map_err(|_| Error::new(5, ErrorKind::InvalidUtf8))?)
        }
        _ => return Err(Error::new(0, ErrorKind::UnknownTag)),
    };
    reader.finish()?;
    Ok(result)
}

pub fn encode_value(value: Option<&str>, output: &mut [u8], max_bytes: usize) -> Result<usize> {
    let len = match value {
        None => 1,
        Some(text) => text.len().checked_add(5).ok_or(Error::new(0, ErrorKind::Overflow))?,
    };
    if len.checked_add(HEADER_BYTES).is_none_or(|total| total > max_bytes) {
        return Err(Error::new(0, ErrorKind::LimitExceeded));
    }
    let mut writer = start(output, 2, len)?;
    match value {
        None => writer.u8(0)?,
        Some(text) => {
            writer.u8(1)?;
            writer.u32(u32::try_from(text.len()).map_err(|_| Error::new(0, ErrorKind::Overflow))?)?;
            writer.put(text.as_bytes())?;
        }
    }
    Ok(writer.len())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Source {
    pub id: SegmentId,
    pub sealed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Manifest<'a> {
    pub generation: Generation,
    entries: &'a [u8],
    count: u32,
}

impl<'a> Manifest<'a> {
    pub fn parse(bytes: &'a [u8], max_sources: u32, max_bytes: usize) -> Result<Self> {
        let mut reader = Reader::new(payload(bytes, 4, max_bytes)?);
        let generation = Generation::new(reader.u64()?).map_err(|_| Error::new(0, ErrorKind::InvalidValue))?;
        let count = reader.u32()?;
        if count > max_sources { return Err(Error::new(8, ErrorKind::LimitExceeded)); }
        let entries = reader.take(reader.remaining())?;
        if (entries.len() as u64) != u64::from(count) * 16 {
            return Err(Error::new(12, ErrorKind::InvalidValue));
        }
        let view = Self { generation, entries, count };
        let mut previous = 0;
        for index in 0..count {
            let source = view.source(index)?;
            if source.id.get() <= previous { return Err(Error::new(12, ErrorKind::InvalidOrder)); }
            previous = source.id.get();
        }
        Ok(view)
    }

    pub const fn len(self) -> u32 { self.count }
    pub const fn is_empty(self) -> bool { self.count == 0 }

    pub fn source(self, index: u32) -> Result<Source> {
        if index >= self.count { return Err(Error::new(0, ErrorKind::InvalidValue)); }
        let offset = usize::try_from(index).ok().and_then(|n| n.checked_mul(16))
            .ok_or(Error::new(0, ErrorKind::Overflow))?;
        let mut reader = Reader::new(&self.entries[offset..offset + 16]);
        let id = SegmentId::new(reader.u64()?).map_err(|_| Error::new(offset, ErrorKind::InvalidValue))?;
        let sealed = match reader.u8()? {
            0 => false, 1 => true, _ => return Err(Error::new(offset + 8, ErrorKind::UnknownTag)),
        };
        if reader.take(7)?.iter().any(|&byte| byte != 0) { return Err(Error::new(offset + 9, ErrorKind::NonCanonical)); }
        Ok(Source { id, sealed })
    }
}

pub fn encode_manifest(generation: Generation, sources: &[Source], output: &mut [u8], max_sources: u32) -> Result<usize> {
    let count = u32::try_from(sources.len()).map_err(|_| Error::new(0, ErrorKind::LimitExceeded))?;
    if count > max_sources { return Err(Error::new(0, ErrorKind::LimitExceeded)); }
    let mut previous = 0;
    for source in sources {
        if source.id.get() <= previous { return Err(Error::new(0, ErrorKind::InvalidOrder)); }
        previous = source.id.get();
    }
    let len = sources.len().checked_mul(16).and_then(|n| n.checked_add(12))
        .ok_or(Error::new(0, ErrorKind::Overflow))?;
    let mut writer = start(output, 4, len)?;
    writer.u64(generation.get())?;
    writer.u32(count)?;
    for source in sources {
        writer.u64(source.id.get())?;
        writer.u8(u8::from(source.sealed))?;
        writer.put(&[0; 7])?;
    }
    Ok(writer.len())
}
