//! bounded phrase witnesses from a document prefix; unused tails stay unread.

use super::document::{MAX_DOCUMENT_BYTES, MAX_DOCUMENT_TOKENS, MAX_TERM_BYTES};
use crate::analysis::PROFILE_ID;
use crate::codec::ErrorKind;
use crate::codec::bytes::Reader;
use crate::error::{Error, Result};

#[derive(Clone, Copy)]
struct Stream<'a> {
    reader: Reader<'a>,
    remaining: u32,
    previous: u32,
    first: bool,
    tokens: u32,
    complete: bool,
}

impl Stream<'_> {
    fn next(&mut self) -> Result<Option<u32>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let delta = self.reader.var_u32()?;
        if !self.first && delta == 0 {
            return Err(Error::InvalidDocument);
        }
        let position = self
            .previous
            .checked_add(delta)
            .ok_or(Error::InvalidDocument)?;
        if position >= self.tokens {
            return Err(Error::InvalidDocument);
        }
        self.previous = position;
        self.first = false;
        self.remaining -= 1;
        if self.remaining == 0 && self.complete {
            self.reader.finish()?;
        }
        Ok(Some(position))
    }
}

/// returns unknown when the supplied prefix cannot decide the phrase.
/// only consumed directory entries and deltas are validated; use the full
/// document validator for integrity checking, including unused tails.
pub fn matches(
    bytes: &[u8],
    total: usize,
    tokens: u32,
    terms: u32,
    wanted: &[String],
) -> Result<Option<bool>> {
    if wanted.is_empty()
        || wanted.len() > 64
        || total > MAX_DOCUMENT_BYTES
        || bytes.len() > total
        || tokens > MAX_DOCUMENT_TOKENS
        || terms > tokens
    {
        return Err(Error::InvalidDocument);
    }
    match evaluate(bytes, total, tokens, terms, wanted) {
        Err(Error::Codec(error)) if error.kind == ErrorKind::Truncated && bytes.len() < total => {
            Ok(None)
        }
        Err(Error::Limit("prefix position work")) if bytes.len() < total => Ok(None),
        result => result.map(Some),
    }
}

fn evaluate(
    bytes: &[u8],
    total: usize,
    tokens: u32,
    terms: u32,
    wanted: &[String],
) -> Result<bool> {
    let mut header = Reader::new(bytes);
    if header.take(4)? != b"PD02" || header.u32()? != PROFILE_ID {
        return Err(Error::InvalidProfile);
    }
    if header.u32()? != tokens || header.u32()? != terms {
        return Err(Error::InvalidDocument);
    }
    let mut offset = header.offset();
    let mut previous = "";
    let mut frequency = 0u32;
    let mut streams = [None; 64];
    for _ in 0..terms {
        let input = bytes
            .get(offset..)
            .ok_or_else(|| crate::codec::Error::new(offset, ErrorKind::Truncated))?;
        let mut entry = Reader::new(input);
        let len = usize::from(entry.u16()?);
        if len == 0 || len > MAX_TERM_BYTES || entry.u16()? != 0 {
            return Err(Error::InvalidDocument);
        }
        let size = entry.u32()? as usize;
        let term = std::str::from_utf8(entry.take(len)?).map_err(|_| Error::InvalidDocument)?;
        if term <= previous || size < 4 {
            return Err(Error::InvalidDocument);
        }
        previous = term;
        let start = offset
            .checked_add(entry.offset())
            .ok_or(Error::InvalidDocument)?;
        let end = start.checked_add(size).ok_or(Error::InvalidDocument)?;
        if end > total {
            return Err(Error::InvalidDocument);
        }
        let count = entry.u32()?;
        if count == 0 || count > tokens || count as usize > size - 4 {
            return Err(Error::InvalidDocument);
        }
        frequency = frequency.checked_add(count).ok_or(Error::InvalidDocument)?;
        if frequency > tokens {
            return Err(Error::InvalidDocument);
        }
        for (index, name) in wanted.iter().enumerate() {
            if name == term {
                streams[index] = Some(Stream {
                    reader: Reader::new(&bytes[start + 4..end.min(bytes.len())]),
                    remaining: count,
                    previous: 0,
                    first: true,
                    tokens,
                    complete: end <= bytes.len(),
                });
            }
        }
        if streams[..wanted.len()].iter().all(Option::is_some) {
            let complete = streams[..wanted.len()]
                .iter()
                .all(|stream| stream.is_some_and(|stream| stream.complete));
            return witness(
                &mut streams[..wanted.len()],
                if complete { usize::MAX } else { 256 },
            );
        }
        offset = end;
    }
    if offset != total || frequency != tokens {
        return Err(Error::InvalidDocument);
    }
    Ok(false)
}

fn witness(streams: &mut [Option<Stream<'_>>], mut work: usize) -> Result<bool> {
    let anchor = streams
        .iter()
        .enumerate()
        .min_by_key(|(_, s)| s.as_ref().map_or(u32::MAX, |s| s.remaining))
        .map(|(index, _)| index)
        .ok_or(Error::InvalidState)?;
    let mut current = [None; 64];
    while let Some(position) = advance(
        streams[anchor].as_mut().ok_or(Error::InvalidState)?,
        &mut work,
    )? {
        let Some(start) = position.checked_sub(anchor as u32) else {
            continue;
        };
        let mut matched = true;
        for index in 0..streams.len() {
            if index == anchor {
                continue;
            }
            let target = start
                .checked_add(index as u32)
                .ok_or(Error::InvalidDocument)?;
            let stream = streams[index].as_mut().ok_or(Error::InvalidState)?;
            loop {
                let value = match current[index] {
                    Some(value) => Some(value),
                    None => advance(stream, &mut work)?,
                };
                current[index] = value;
                match value {
                    Some(value) if value < target => current[index] = None,
                    Some(value) if value == target => break,
                    None => return Ok(false),
                    _ => {
                        matched = false;
                        break;
                    }
                }
            }
            if !matched {
                break;
            }
        }
        if matched {
            return Ok(true);
        }
    }
    Ok(false)
}

fn advance(stream: &mut Stream<'_>, work: &mut usize) -> Result<Option<u32>> {
    *work = work
        .checked_sub(1)
        .ok_or(Error::Limit("prefix position work"))?;
    stream.next()
}
