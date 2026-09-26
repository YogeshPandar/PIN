use super::Extent;
use crate::error::{Error, Result};
use crate::mutable::page::{NO_BLOCK, PRIMARY_PAYLOAD_BYTES, Page};

/// packs many immutable containers into one standard postgres page payload.
pub struct PrimaryArena {
    block: u32,
    bytes: Vec<u8>,
}

impl PrimaryArena {
    pub fn new(block: u32) -> Result<Self> {
        if block == 0 || block == NO_BLOCK {
            return Err(Error::InvalidParameters);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(PRIMARY_PAYLOAD_BYTES)
            .map_err(|_| Error::Allocation)?;
        Ok(Self { block, bytes })
    }

    /// returns none without modifying the arena when the next page is needed.
    pub fn try_push(&mut self, payload: &[u8]) -> Result<Option<Extent>> {
        if payload.is_empty() || payload.len() > PRIMARY_PAYLOAD_BYTES {
            return Err(Error::InvalidParameters);
        }
        let Some(end) = self.bytes.len().checked_add(payload.len()) else {
            return Err(Error::InvalidState);
        };
        if end > PRIMARY_PAYLOAD_BYTES {
            return Ok(None);
        }
        let offset = 16 + self.bytes.len();
        self.bytes.extend_from_slice(payload);
        Ok(Some(Extent {
            block: self.block,
            offset: offset as u16,
            len: payload.len() as u16,
        }))
    }

    pub fn finish(self) -> Result<Page> {
        Page::primary(self.block, &self.bytes)
    }
}
