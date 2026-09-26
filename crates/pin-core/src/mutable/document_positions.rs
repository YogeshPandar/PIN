//! versioned document position views; complete readers validate both encodings.
use super::MAX_DOCUMENT_TOKENS;
use crate::codec::bytes::Reader;
use crate::codec::position_blocks::{PositionBlockIter, PositionBlocks};
use crate::codec::positions::{PositionIter, Positions};
use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug)]
pub enum DocumentPositions<'a> {
    Deltas(Positions<'a>),
    Blocks(PositionBlocks<'a>),
}

impl<'a> DocumentPositions<'a> {
    pub(super) fn parse(bytes: &'a [u8], blocked: bool) -> Result<Self> {
        if blocked {
            let view = PositionBlocks::open(bytes, MAX_DOCUMENT_TOKENS)?;
            view.validate_all()?;
            Ok(Self::Blocks(view))
        } else {
            Ok(Self::Deltas(Positions::parse(bytes, MAX_DOCUMENT_TOKENS)?))
        }
    }

    pub(super) fn count(bytes: &[u8], blocked: bool, tokens: u32) -> Result<u32> {
        let count = if blocked {
            PositionBlocks::open(bytes, tokens)?.len()
        } else {
            let mut reader = Reader::new(bytes);
            let count = reader.u32()?;
            if count as usize > reader.remaining() {
                return Err(Error::InvalidDocument);
            }
            count
        };
        if count == 0 || count > tokens {
            return Err(Error::InvalidDocument);
        }
        Ok(count)
    }

    pub fn len(self) -> u32 {
        match self {
            Self::Deltas(v) => v.len(),
            Self::Blocks(v) => v.len(),
        }
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn iter(self) -> DocumentPositionIter<'a> {
        match self {
            Self::Deltas(v) => DocumentPositionIter::Deltas(v.iter()),
            Self::Blocks(v) => DocumentPositionIter::Blocks(v.iter()),
        }
    }
}

pub enum DocumentPositionIter<'a> {
    Deltas(PositionIter<'a>),
    Blocks(PositionBlockIter<'a>),
}
impl Iterator for DocumentPositionIter<'_> {
    type Item = crate::codec::Result<u32>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Deltas(v) => v.next(),
            Self::Blocks(v) => v.next(),
        }
    }
}
impl std::iter::FusedIterator for DocumentPositionIter<'_> {}
