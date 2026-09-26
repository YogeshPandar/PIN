use crate::codec::bytes::{Reader, Writer};
use crate::error::{Error, Result};
use crate::identity::{Generation, HeapLayout};
use crate::mutable::page::NO_BLOCK;

const MAGIC: &[u8; 4] = b"PNR2";
const VERSION: u16 = 1;
pub const ROOT_BYTES: usize = 48;

/// one published v2 manifest pointer, scoped by its never-reused relation generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrimaryRoot {
    pub relation: Generation,
    pub layout: HeapLayout,
    pub epoch: u64,
    pub manifest_block: u32,
    pub segment_count: u32,
}

impl PrimaryRoot {
    pub fn empty(relation: Generation, layout: HeapLayout) -> Self {
        Self {
            relation,
            layout,
            epoch: 1,
            manifest_block: NO_BLOCK,
            segment_count: 0,
        }
    }

    fn valid(self) -> bool {
        self.epoch != 0
            && ((self.segment_count == 0 && self.manifest_block == NO_BLOCK)
                || (self.segment_count != 0
                    && self.manifest_block != 0
                    && self.manifest_block != NO_BLOCK))
    }

    pub fn encode(self) -> Result<[u8; ROOT_BYTES]> {
        if !self.valid() {
            return Err(Error::InvalidParameters);
        }
        let mut bytes = [0u8; ROOT_BYTES];
        let mut writer = Writer::new(&mut bytes);
        writer.put(MAGIC)?;
        writer.u16(VERSION)?;
        writer.u16(0)?;
        writer.u32(ROOT_BYTES as u32)?;
        writer.u16(self.layout.max_offset())?;
        writer.u16(0)?;
        writer.u64(self.relation.get())?;
        writer.u64(self.epoch)?;
        writer.u32(self.manifest_block)?;
        writer.u32(self.segment_count)?;
        writer.u64(0)?;
        debug_assert_eq!(writer.len(), ROOT_BYTES);
        Ok(bytes)
    }

    pub fn open(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != ROOT_BYTES {
            return Err(Error::InvalidState);
        }
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != MAGIC
            || reader.u16()? != VERSION
            || reader.u16()? != 0
            || reader.u32()? != ROOT_BYTES as u32
        {
            return Err(Error::InvalidState);
        }
        let layout = HeapLayout::new(reader.u16()?).map_err(|_| Error::InvalidState)?;
        if reader.u16()? != 0 {
            return Err(Error::InvalidState);
        }
        let relation = Generation::new(reader.u64()?).map_err(|_| Error::InvalidState)?;
        let epoch = reader.u64()?;
        let manifest_block = reader.u32()?;
        let segment_count = reader.u32()?;
        if reader.u64()? != 0 {
            return Err(Error::InvalidState);
        }
        reader.finish()?;
        let root = Self {
            relation,
            layout,
            epoch,
            manifest_block,
            segment_count,
        };
        if !root.valid() {
            return Err(Error::InvalidState);
        }
        Ok(root)
    }
}
