// structured pure-engine errors; formatting occurs only when the caller requests it.

use crate::budget::BudgetError;
use crate::codec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Allocation,
    Limit(&'static str),
    QuerySyntax { offset: usize },
    InvalidProfile,
    InvalidDocument,
    DuplicateDocument,
    ForeignRelation,
    InvalidParameters,
    InvalidState,
    InvalidStatistics,
    NonFiniteScore,
    Budget(BudgetError),
    Codec(codec::Error),
}

impl From<BudgetError> for Error {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<codec::Error> for Error {
    fn from(error: codec::Error) -> Self {
        Self::Codec(error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
