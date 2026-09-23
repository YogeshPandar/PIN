# G9 physical integration

The scalar kernels support `no_std` with `--no-default-features`. The default
`std` feature retains the existing checked runtime AVX2 selection. The grouped
path selects scalar operations. PostgreSQL orchestration remains a host boundary,
not a freestanding program.

## Selected protocol

A maintenance operation holds the exclusive structural barrier before the writer
interlock. It captures complete published owners and their complete term coverage.
PostgreSQL's bounded tuplesort orders fixed-width records; it owns temporary spill
files. Immutable catalog nodes and versioned record fragments are written through
the existing PostgreSQL buffer and generic-WAL adapter.

The grouped snapshot is supplemental during migration. Canonical owner records,
legacy postings and positional payloads remain readable. An atomic metadata swap
publishes the snapshot and its owner cutoff. Newer owners remain immediately
searchable through the legacy executor. Results from complete generations are
unioned only after Boolean evaluation. Term masks from different generations are
never intersected.

A durable build journal owns unpublished output. Publication changes that journal
to the old snapshot's retirement chain in the same WAL record. Recovery discards
unpublished output or completes retirement. IDs use the existing durable,
nonwrapping identity allocator and are never reused after a failed build.

VACUUM clears every published shared-liveness copy before returning permission to
recycle heap slots. It does not flush WAL for each bit. Existing writer exclusion,
ordered buffer locks, standard-page boundaries and WAL-before-data ordering still
apply. PostgreSQL alone checks snapshot visibility.

## Activation and qualification

The SQL path must remain default-off. New physical storage, catalog traversal,
publication/recovery, VACUUM retirement, mixed-format queries and SQL/crash tests
are under implementation. This document does not certify any unimplemented step
or claim a benchmark improvement. The PR records executable CI results by commit.

## Official references

- PostgreSQL 18: https://www.postgresql.org/docs/18/index-scanning.html
- PostgreSQL 18: https://www.postgresql.org/docs/18/index-locking.html
- PostgreSQL 18: https://www.postgresql.org/docs/18/generic-wal.html
- PostgreSQL 18.6 tuplesort declarations:
  https://github.com/postgres/postgres/blob/724edf9bde9d356724ad384a2e196edc3c9f80f7/src/include/utils/tuplesort.h
- Rust core: https://doc.rust-lang.org/core/index.html
- Rust arrays: https://doc.rust-lang.org/core/array/fn.from_fn.html
- Rust integer encoding: https://doc.rust-lang.org/core/primitive.u64.html
