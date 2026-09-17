// checked experimental payloads; caller-owned buffers, no host pointers.
// contracts: docs/g1-format.md and docs/g1-api-evidence.md.

pub mod bytes;
pub mod dictionary;
pub mod offsets;
pub mod positions;
pub mod records;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Truncated,
    TrailingBytes,
    Overflow,
    NonCanonical,
    InvalidValue,
    InvalidOrder,
    InvalidUtf8,
    LimitExceeded,
    BadMagic,
    UnsupportedVersion,
    UnsupportedFeatures,
    UnknownTag,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub offset: usize,
    pub kind: ErrorKind,
}

impl Error {
    pub const fn new(offset: usize, kind: ErrorKind) -> Self {
        Self { offset, kind }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
