#!/usr/bin/env bash
# Qualify the v2 selected-extent PostgreSQL buffer path in a fresh PG18 cluster.
set -euo pipefail
: "${PGRX_PG_CONFIG_PATH:?set the PostgreSQL 18.6 pg_config path}"
: "${PIN_TEST_PACKAGE:?set the pgrx test-hooks package directory}"
: "${PIN_OUTPUT:?set a new output directory for qualification logs}"

if [[ $(id -u) == 0 ]]; then
    echo 'Run the v2 extent qualification as an unprivileged user.' >&2
    exit 2
fi
if [[ -e "$PIN_OUTPUT" ]]; then
    echo "output path already exists: $PIN_OUTPUT" >&2
    exit 2
fi
mkdir -p "$PIN_OUTPUT"
package=$(realpath "$PIN_TEST_PACKAGE")
bin=$("$PGRX_PG_CONFIG_PATH" --bindir)
if [[ $("$PGRX_PG_CONFIG_PATH" --version) != 'PostgreSQL 18.6'* ]]; then
    echo 'v2 extent qualification requires PostgreSQL 18.6.' >&2
    exit 2
fi
test -f "$package/usr/lib/postgresql/18/lib/pin.so"
test -f "$package/usr/share/postgresql/18/extension/pin.control"

work=$(mktemp -d /tmp/pin-v2-extent.XXXXXXXX)
port=${PIN_TEST_PORT:-55491}
cleanup() {
    "$bin/pg_ctl" -D "$work/data" -m immediate stop >/dev/null 2>&1 || true
    rm -rf -- "$work"
}
trap cleanup EXIT

mkdir -p "$work/socket"
unset PGHOST PGHOSTADDR PGPORT PGDATABASE PGUSER PGSERVICE PGSERVICEFILE PGPASSFILE PGOPTIONS PGAPPNAME
"$bin/initdb" -D "$work/data" --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject \
    >"$PIN_OUTPUT/initdb.log" 2>&1
cat >> "$work/data/postgresql.conf" <<CONF
shared_preload_libraries = 'pin'
extension_control_path = '$package/usr/share/postgresql/18:\$system'
dynamic_library_path = '$package/usr/lib/postgresql/18/lib:\$libdir'
listen_addresses = ''
unix_socket_directories = '$work/socket'
port = $port
fsync = on
full_page_writes = on
autovacuum = off
statement_timeout = '30s'
lock_timeout = '10s'
CONF

"$bin/pg_ctl" -D "$work/data" -l "$PIN_OUTPUT/postgres.log" -w start \
    >"$PIN_OUTPUT/start.log" 2>&1
psql=("$bin/psql" -X -h "$work/socket" -p "$port" -d postgres -v ON_ERROR_STOP=1)
"${psql[@]}" -c 'CREATE EXTENSION pin;' >"$PIN_OUTPUT/extension.log" 2>&1
"${psql[@]}" -c "
CREATE TABLE v2_extent_docs(body text);
INSERT INTO v2_extent_docs
SELECT 'document ' || g || ' ' || repeat(md5(g::text), 2)
FROM generate_series(1, 32) AS g;
CREATE INDEX v2_extent_idx ON v2_extent_docs USING pin(body);
SELECT pin.v2_primary_extent_roundtrip('v2_extent_idx'::regclass::oid);
" >"$PIN_OUTPUT/roundtrip.log" 2>&1

# Catch the expected XX002 inside a PL/pgSQL subtransaction, then call the
# successful fast path again in the same backend. This checks buffer/error
# cleanup without exposing arbitrary pointers or SQL text to a test hook.
"${psql[@]}" -c "
DO \$pin\$
DECLARE caught_state text;
        caught_message text;
BEGIN
  BEGIN
    PERFORM pin.v2_primary_extent_wrong_tag('v2_extent_idx'::regclass::oid);
    caught_message := '';
  EXCEPTION WHEN OTHERS THEN
    GET STACKED DIAGNOSTICS
      caught_state = RETURNED_SQLSTATE,
      caught_message = MESSAGE_TEXT;
  END;
  IF caught_state <> 'XX002' OR
     caught_message <> 'Pin index has an invalid PostgreSQL page header' THEN
    RAISE EXCEPTION 'wrong-tag probe returned unexpected result: %, %',
      caught_state, caught_message;
  END IF;
  RAISE NOTICE 'caught expected PostgreSQL error %: %', caught_state, caught_message;
END
\$pin\$;
SELECT pin.v2_primary_extent_roundtrip('v2_extent_idx'::regclass::oid);
" >"$PIN_OUTPUT/error-cleanup.log" 2>&1

grep -q '^ t$' "$PIN_OUTPUT/roundtrip.log" || {
    echo 'native roundtrip did not return true' >&2
    cat "$PIN_OUTPUT/roundtrip.log" >&2
    exit 1
}
grep -q '^ t$' "$PIN_OUTPUT/error-cleanup.log" || {
    echo 'post-error native roundtrip did not return true' >&2
    cat "$PIN_OUTPUT/error-cleanup.log" >&2
    exit 1
}
echo 'PASS: selected primary extent matched the full page; sentinels were preserved; XX002 was caught and a subsequent read succeeded.'
