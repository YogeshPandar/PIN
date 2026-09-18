// little-endian fields and canonical u32/u64 varints over borrowed byte slices.
// reads validate extents before advancing; failed writes may alter the output.
// contract: https://doc.rust-lang.org/std/primitive.u32.html#method.from_le_bytes

use super::{Error, ErrorKind, Result};

#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub const fn offset(&self) -> usize {
        self.offset
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    // returns a borrowed extent; overflow and truncation leave the cursor unchanged.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(Error::new(self.offset, ErrorKind::Overflow))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(Error::new(self.offset, ErrorKind::Truncated))?;
        self.offset = end;
        Ok(value)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::new(self.offset, ErrorKind::Truncated))
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    // rejects overflow and redundant groups; failure leaves the cursor unchanged.
    pub fn var_u32(&mut self) -> Result<u32> {
        let mut input = *self;
        let start = input.offset;
        let mut value = 0u32;
        for group in 0..5 {
            let byte = input.u8()?;
            if group == 4 && byte > 15 {
                return Err(Error::new(start, ErrorKind::Overflow));
            }
            value |= u32::from(byte & 127) << (group * 7);
            if byte & 128 == 0 {
                if group != 0 && byte == 0 {
                    return Err(Error::new(start, ErrorKind::NonCanonical));
                }
                *self = input;
                return Ok(value);
            }
        }
        Err(Error::new(start, ErrorKind::Overflow))
    }

    // the tenth group has one payload bit; failed reads preserve the cursor.
    pub fn var_u64(&mut self) -> Result<u64> {
        let mut input = *self;
        let start = input.offset;
        let mut value = 0u64;
        for group in 0..10 {
            let byte = input.u8()?;
            if group == 9 && byte > 1 {
                return Err(Error::new(start, ErrorKind::Overflow));
            }
            value |= u64::from(byte & 127) << (group * 7);
            if byte & 128 == 0 {
                if group != 0 && byte == 0 {
                    return Err(Error::new(start, ErrorKind::NonCanonical));
                }
                *self = input;
                return Ok(value);
            }
        }
        Err(Error::new(start, ErrorKind::Overflow))
    }

    pub fn finish(self) -> Result<()> {
        if self.remaining() != 0 {
            return Err(Error::new(self.offset, ErrorKind::TrailingBytes));
        }
        Ok(())
    }
}

pub struct Writer<'a> {
    bytes: &'a mut [u8],
    offset: usize,
}

impl<'a> Writer<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub const fn len(&self) -> usize {
        self.offset
    }

    pub const fn is_empty(&self) -> bool {
        self.offset == 0
    }

    pub fn put(&mut self, value: &[u8]) -> Result<()> {
        let end = self
            .offset
            .checked_add(value.len())
            .ok_or(Error::new(self.offset, ErrorKind::Overflow))?;
        let target = self
            .bytes
            .get_mut(self.offset..end)
            .ok_or(Error::new(self.offset, ErrorKind::Truncated))?;
        target.copy_from_slice(value);
        self.offset = end;
        Ok(())
    }

    pub fn u8(&mut self, value: u8) -> Result<()> {
        self.put(&[value])
    }

    pub fn u16(&mut self, value: u16) -> Result<()> {
        self.put(&value.to_le_bytes())
    }

    pub fn u32(&mut self, value: u32) -> Result<()> {
        self.put(&value.to_le_bytes())
    }

    pub fn u64(&mut self, value: u64) -> Result<()> {
        self.put(&value.to_le_bytes())
    }

    pub fn var_u64(&mut self, mut value: u64) -> Result<()> {
        let mut bytes = [0u8; 10];
        let mut len = 0;
        loop {
            let low = (value & 127) as u8;
            value >>= 7;
            bytes[len] = low | if value == 0 { 0 } else { 128 };
            len += 1;
            if value == 0 {
                return self.put(&bytes[..len]);
            }
        }
    }

    pub fn var_u32(&mut self, mut value: u32) -> Result<()> {
        let mut bytes = [0u8; 5];
        let mut len = 0;
        loop {
            let low = (value & 127) as u8;
            value >>= 7;
            bytes[len] = low | if value == 0 { 0 } else { 128 };
            len += 1;
            if value == 0 {
                return self.put(&bytes[..len]);
            }
        }
    }
}

pub const fn var_u32_len(value: u32) -> usize {
    if value == 0 {
        1
    } else {
        (32 - value.leading_zeros()).div_ceil(7) as usize
    }
}
