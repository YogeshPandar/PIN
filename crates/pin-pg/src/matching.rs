//! versioned SQL query domain and exact sequential predicate.
//! SQL and bitmap rechecks share the G1 profile, parser, limits and document oracle.
//! no index storage or visibility state is consulted here.

use pgrx::prelude::*;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};

pub(crate) const QUERY_MEMORY: usize = 1 << 20;
pub(crate) const PREPARE_MEMORY: usize = 32 << 20;
pub(crate) const MATCH_STEPS: usize = 1 << 24;
const PARTICIPANT_FIXED_MEMORY: usize = 64 << 10;

pub(crate) fn build_participant_memory() -> Result<usize> {
    PREPARE_MEMORY
        .checked_add(AnalysisLimits::default().input_bytes)
        .and_then(|bytes| bytes.checked_add(PARTICIPANT_FIXED_MEMORY))
        .ok_or(Error::Limit("parallel build memory"))
}

pub(crate) fn count_participant_memory() -> Result<usize> {
    let limits = AnalysisLimits::default();
    QUERY_MEMORY
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(limits.memory_bytes))
        .and_then(|bytes| bytes.checked_add(limits.input_bytes))
        .and_then(|bytes| bytes.checked_add(PARTICIPANT_FIXED_MEMORY))
        .ok_or(Error::Limit("parallel count memory"))
}

pub(crate) fn input<T>(result: Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => raise(error, false),
    }
}

pub(crate) fn stored<T>(result: Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => raise(error, true),
    }
}

fn raise(error: Error, stored: bool) -> ! {
    let code = match error {
        Error::Allocation => PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY,
        Error::Limit(_) | Error::Budget(_) => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
        _ if stored => PgSqlErrorCode::ERRCODE_INDEX_CORRUPTED,
        _ => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
    };
    pgrx::ereport!(ERROR, code, format!("Pin: {error}"));
}

#[pg_extern(immutable, strict, parallel_safe)]
fn query_valid(bytes: &[u8]) -> bool {
    Query::decode(bytes, QueryLimits::default()).is_ok()
}

pgrx::extension_sql!(
    "CREATE DOMAIN pin.query AS bytea CHECK (pin.query_valid(VALUE));",
    name = "pin_query_domain",
    requires = [query_valid]
);

#[pg_extern(requires = ["pin_query_domain"], sql = r#"
CREATE FUNCTION pin.parse_query(text) RETURNS pin.query
LANGUAGE c IMMUTABLE STRICT PARALLEL SAFE
AS '@MODULE_PATHNAME@', '@FUNCTION_NAME@';
"#)]
fn parse_query(source: &str) -> Vec<u8> {
    crate::compatibility::database();
    let query = input(Query::parse(source, QueryLimits::default()));
    let length = source.len() + 28;
    let mut bytes = Vec::new();
    input(
        bytes
            .try_reserve_exact(length)
            .map_err(|_| Error::Allocation),
    );
    if bytes.capacity() > QUERY_MEMORY {
        input::<()>(Err(Error::Limit("encoded query memory")));
    }
    bytes.resize(length, 0);
    let written = input(query.encode(&mut bytes, QUERY_MEMORY));
    bytes.truncate(written);
    bytes
}

#[pg_extern(requires = ["pin_query_domain"], sql = r#"
CREATE FUNCTION pin.matches(text, pin.query) RETURNS boolean
LANGUAGE c IMMUTABLE STRICT PARALLEL SAFE
AS '@MODULE_PATHNAME@', '@FUNCTION_NAME@';
"#)]
fn matches(body: &str, bytes: &[u8]) -> bool {
    crate::compatibility::database();
    crate::storage::interrupt();
    let query = input(Query::decode(bytes, QueryLimits::default()));
    let document = input(Analyzed::analyze(body, AnalysisLimits::default()));
    let result = input(oracle::matches(
        &document,
        &query,
        QUERY_MEMORY,
        MATCH_STEPS,
    ));
    crate::storage::interrupt();
    result
}

pgrx::extension_sql!(
    r#"
CREATE OPERATOR pin.@@@ (
    LEFTARG = text, RIGHTARG = pin.query, PROCEDURE = pin.matches
);
CREATE OPERATOR CLASS pin.text_ops DEFAULT FOR TYPE text USING pin AS
    OPERATOR 1 pin.@@@ (text, pin.query);
"#,
    name = "pin_text_ops",
    requires = ["pin_access_method", matches, parse_query]
);
