#!/usr/bin/env bash
# Qualify the v2 primary bytea sorter in a fresh PostgreSQL 18 cluster.
set -euo pipefail
: "${PGRX_PG_CONFIG_PATH:?set the PostgreSQL 18.6 pg_config path}"
: "${PIN_TEST_PACKAGE:?set the pgrx test-hooks package directory}"
: "${PIN_OUTPUT:?set a new output directory for qualification logs}"

if [[ $(id -u) == 0 ]]; then
    echo 'Run the v2 sorter qualification as an unprivileged user.' >&2
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
    echo 'v2 sorter qualification requires PostgreSQL 18.6.' >&2
    exit 2
fi
test -f "$package/usr/lib/postgresql/18/lib/pin.so"
test -f "$package/usr/share/postgresql/18/extension/pin.control"

work=$(mktemp -d /tmp/pin-v2-sort.XXXXXXXX)
port=${PIN_TEST_PORT:-55492}
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
maintenance_work_mem = '1MB'
statement_timeout = '60s'
lock_timeout = '10s'
CONF

"$bin/pg_ctl" -D "$work/data" -l "$PIN_OUTPUT/postgres.log" -w start \
    >"$PIN_OUTPUT/start.log" 2>&1
psql=("$bin/psql" -X -h "$work/socket" -p "$port" -d postgres -v ON_ERROR_STOP=1)
"${psql[@]}" -c 'CREATE EXTENSION pin;' >"$PIN_OUTPUT/extension.log" 2>&1
"${psql[@]}" -c 'SELECT pin.v2_primary_sort_qualification();' \
    >"$PIN_OUTPUT/sort-qualification.log" 2>&1
grep -q '^ t$' "$PIN_OUTPUT/sort-qualification.log" || {
    echo 'primary sort qualification did not return true' >&2
    cat "$PIN_OUTPUT/sort-qualification.log" >&2
    exit 1
}
echo 'PASS: variable bytea keys sorted exactly, duplicate-term roots were preserved, EOF was observed, and PostgreSQL reported disk spill.'
