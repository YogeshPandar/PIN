# Primary v2 extent reader native qualification

Source branch: `codex/primary-index-v2` at `55f34b8` plus the test-only hook and
harness in this checkpoint. PostgreSQL 18.6, fresh disposable C-locale cluster,
one synthetic table with 32 rows and a PIN index. The harness stopped and
removed the cluster after the run. Raw `initdb`, startup, extension, SQL, and
server logs are in `raw/`; the command is `tools/v2_extent_native.sh`.
The full `pin-core` suite also passed: 223 passed, 0 failed, 3 existing
Unicode-data tests ignored (`raw/core-suite.log`).

The test-only hook appended a `Primary` page with an 8,152-byte private payload
through the real PostgreSQL Generic WAL adapter. It compared a 13-byte extent
at private-page offset 32 from `PgStore::read_primary_extent` to the same bytes
from a full `PgStore::read`. Both two-byte guard regions retained `0xA5`.
`raw/roundtrip.log` records `t`. A second hook read an ordinary non-Primary
index page as a Primary extent; PostgreSQL raised `XX002` with the expected
corruption message. PL/pgSQL caught the error and a subsequent roundtrip in
the same backend returned `t` (`raw/error-cleanup.log`). This exercises the
new reader's error cleanup and proves the backend remained usable.

The package was built with:

```sh
PGRX_PG_CONFIG_PATH=/usr/lib/postgresql/18/bin/pg_config \
PIN_BUILD_REVISION=55f34b8 \
cargo pgrx package -p pin-pg --features test-hooks \
  --pg-config /usr/lib/postgresql/18/bin/pg_config \
  --out-dir /tmp/pin-v2-extent-package-max
PGRX_PG_CONFIG_PATH=/usr/lib/postgresql/18/bin/pg_config \
PIN_TEST_PACKAGE=/tmp/pin-v2-extent-package-max \
PIN_OUTPUT=/tmp/pin-v2-extent-run4 tools/v2_extent_native.sh
```

The extension was compiled from the local uncommitted hook files as well as
the recorded source commit. The run validates byte copying and error recovery,
not a SQL search or CPU speedup. PostgreSQL still reads and caches an 8 KiB
buffer page. V2 manifest/build, SQL scans, MVCC/VACUUM lifecycle, and paired
GIN/TIN performance remain open.
