# Native PostgreSQL primary sorter qualification

This run qualifies the PostgreSQL host sorter for `TermSortRecord` keys against
a fresh PostgreSQL 18.6 cluster. The `test-hooks` only SQL entry inserts 200,007
keys in unsorted order: 200,000 generated term/root records and seven fixtures
covering variable term lengths, two distinct roots for one term, and one exact
duplicate. It compares all returned keys against Rust's bytewise ordering,
validates each decoded record, checks EOF, and requires PostgreSQL's tuplesort
instrumentation to report disk spill.

The fresh cluster used `maintenance_work_mem = '1MB'`, `fsync = on`, and
`full_page_writes = on`. The harness stopped and removed the disposable data
directory after success. It requires an already-built `test-hooks` package and
does not install that package into the system PostgreSQL directories.

## Reproduction

```sh
PGRX_PG_CONFIG_PATH=/usr/bin/pg_config \
PIN_TEST_PACKAGE=/tmp/pin-v2-sort-package \
PIN_OUTPUT="$PWD/docs/runs/2026-09-26-primary-v2-sort/raw" \
PIN_TEST_PORT=55492 \
tools/v2_sort_native.sh
```

The package was built with:

```sh
PGRX_PG_CONFIG_PATH=/usr/bin/pg_config \
cargo pgrx package -p pin-pg --features test-hooks \
  --pg-config /usr/bin/pg_config --out-dir /tmp/pin-v2-sort-package
```

The qualification returned success:

```text
PASS: variable bytea keys sorted exactly, duplicate-term roots were preserved, EOF was observed, and PostgreSQL reported disk spill.
```

`raw/sort-qualification.log` records the SQL function returning `true`; the
other raw files preserve extension creation, server startup, and initdb logs.
