//! Physical coordinates scoped by relation generation and document incarnation.
//! Constructors validate integer domains, not heap existence or MVCC visibility.
//! PostgreSQL contract: storage/itemptr.h at the commit in `docs/api-evidence.md`.
//! Rust contract: <https://doc.rust-lang.org/std/num/struct.NonZero.html>.

use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};

/// Invalid identity input; constructing an identity never allocates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityError {
    InvalidBlock,
    InvalidOffset,
    InvalidLayout,
    ZeroIdentifier,
    GenerationExhausted,
}

/// A validated heap offset domain derived from the supported server headers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeapLayout {
    max_offset: NonZeroU16,
}

impl HeapLayout {
    /// Validates a server-derived bound against the private scratch capacity.
    ///
    /// # Errors
    /// Returns `InvalidLayout` for zero or more than 512 offsets.
    pub const fn new(max_offset: u16) -> Result<Self, IdentityError> {
        if max_offset > 512 {
            return Err(IdentityError::InvalidLayout);
        }
        match NonZeroU16::new(max_offset) {
            Some(max_offset) => Ok(Self { max_offset }),
            None => Err(IdentityError::InvalidLayout),
        }
    }

    pub const fn max_offset(self) -> u16 {
        self.max_offset.get()
    }
}

/// A heap root coordinate, meaningful only with a relation generation.
/// This is not a PostgreSQL `ItemPointerData` or a disk encoding.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RootTid {
    block: u32,
    offset: NonZeroU16,
}

impl RootTid {
    /// Validates a root coordinate without dereferencing a heap page.
    ///
    /// # Errors
    /// Rejects PostgreSQL's invalid block sentinel and offsets outside `layout`.
    pub const fn new(
        block: u32,
        offset: u16,
        layout: HeapLayout,
    ) -> Result<Self, IdentityError> {
        if block == u32::MAX {
            return Err(IdentityError::InvalidBlock);
        }
        if offset > layout.max_offset() {
            return Err(IdentityError::InvalidOffset);
        }
        match NonZeroU16::new(offset) {
            Some(offset) => Ok(Self { block, offset }),
            None => Err(IdentityError::InvalidOffset),
        }
    }

    pub const fn block(self) -> u32 {
        self.block
    }

    pub const fn offset(self) -> u16 {
        self.offset.get()
    }

    /// Returns an ordered logical key; this is not an on-disk representation.
    pub const fn key(self) -> u64 {
        ((self.block as u64) << 16) | self.offset.get() as u64
    }
}

/// A fetched tuple coordinate, deliberately distinct from its HOT root.
/// Construction alone does not certify snapshot visibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisibleTid(RootTid);

impl VisibleTid {
    /// Validates the coordinate reported by a heap fetch.
    ///
    /// # Errors
    /// Has the same domain errors as `RootTid::new`.
    pub const fn new(
        block: u32,
        offset: u16,
        layout: HeapLayout,
    ) -> Result<Self, IdentityError> {
        match RootTid::new(block, offset, layout) {
            Ok(tid) => Ok(Self(tid)),
            Err(error) => Err(error),
        }
    }

    pub const fn block(self) -> u32 {
        self.0.block()
    }

    pub const fn offset(self) -> u16 {
        self.0.offset()
    }
}

macro_rules! identifier {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Validates a nonzero identifier without allocation.
            ///
            /// # Errors
            /// Returns `ZeroIdentifier` for zero.
            pub const fn new(value: u64) -> Result<Self, IdentityError> {
                match NonZeroU64::new(value) {
                    Some(value) => Ok(Self(value)),
                    None => Err(IdentityError::ZeroIdentifier),
                }
            }

            pub const fn get(self) -> u64 {
                self.0.get()
            }

            /// Advances without wrapping or reusing a previous identifier.
            ///
            /// # Errors
            /// Returns `GenerationExhausted` at `u64::MAX`.
            pub const fn next(self) -> Result<Self, IdentityError> {
                match self.get().checked_add(1) {
                    Some(next) => Self::new(next),
                    None => Err(IdentityError::GenerationExhausted),
                }
            }
        }
    };
}

identifier!(SegmentId, "A source identifier within one relation generation.");
identifier!(Incarnation, "A document incarnation; never inferred from a bare TID.");
identifier!(Generation, "A checked generation counter; not a transaction timestamp.");

/// A database-local physical relation locator and its durable Pin generation.
/// Caller must provide the effective tablespace OID, not a zero default marker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RelationGeneration {
    database: NonZeroU32,
    tablespace: NonZeroU32,
    relfilenumber: NonZeroU32,
    generation: Generation,
}

impl RelationGeneration {
    /// Validates a complete physical identity. No catalog lookup occurs.
    ///
    /// # Errors
    /// Returns `ZeroIdentifier` for a zero database, tablespace, or file number.
    pub const fn new(
        database: u32,
        tablespace: u32,
        relfilenumber: u32,
        generation: Generation,
    ) -> Result<Self, IdentityError> {
        match (
            NonZeroU32::new(database),
            NonZeroU32::new(tablespace),
            NonZeroU32::new(relfilenumber),
        ) {
            (Some(database), Some(tablespace), Some(relfilenumber)) => Ok(Self {
                database,
                tablespace,
                relfilenumber,
                generation,
            }),
            _ => Err(IdentityError::ZeroIdentifier),
        }
    }
}

/// A source-owned document, not merely the current occupant of a heap slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DocumentRef {
    pub relation: RelationGeneration,
    pub segment: SegmentId,
    pub incarnation: Incarnation,
    pub root: RootTid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_boundaries() {
        let layout = HeapLayout::new(128).unwrap();
        assert_eq!(HeapLayout::new(0), Err(IdentityError::InvalidLayout));
        assert_eq!(HeapLayout::new(513), Err(IdentityError::InvalidLayout));
        assert_eq!(RootTid::new(0, 0, layout), Err(IdentityError::InvalidOffset));
        assert_eq!(RootTid::new(0, 129, layout), Err(IdentityError::InvalidOffset));
        assert_eq!(RootTid::new(u32::MAX, 1, layout), Err(IdentityError::InvalidBlock));
        for block in [0, 1, 255, 256, u32::MAX - 1] {
            for offset in 1..=layout.max_offset() {
                let tid = RootTid::new(block, offset, layout).unwrap();
                assert_eq!(tid.block(), block);
                assert_eq!(tid.offset(), offset);
                assert_eq!(tid.key(), (u64::from(block) << 16) | u64::from(offset));
            }
        }
    }

    #[test]
    fn generations_do_not_wrap() {
        assert_eq!(Generation::new(0), Err(IdentityError::ZeroIdentifier));
        let last = Generation::new(u64::MAX).unwrap();
        assert_eq!(last.next(), Err(IdentityError::GenerationExhausted));
        assert_eq!(Generation::new(7).unwrap().next().unwrap().get(), 8);
    }

    #[test]
    fn slot_reuse_and_rewrites_change_identity() {
        let layout = HeapLayout::new(128).unwrap();
        let relation = RelationGeneration::new(1, 2, 3, Generation::new(1).unwrap()).unwrap();
        let old = DocumentRef {
            relation,
            segment: SegmentId::new(1).unwrap(),
            incarnation: Incarnation::new(1).unwrap(),
            root: RootTid::new(42, 7, layout).unwrap(),
        };
        let replacement = DocumentRef {
            incarnation: Incarnation::new(2).unwrap(),
            ..old
        };
        assert_ne!(old, replacement);
        let rewritten = DocumentRef {
            relation: RelationGeneration::new(1, 2, 3, Generation::new(2).unwrap()).unwrap(),
            ..old
        };
        assert_ne!(old, rewritten);
        let visible = VisibleTid::new(42, 8, layout).unwrap();
        assert_eq!(old.root.offset(), 7);
        assert_eq!(visible.offset(), 8);
    }
}
