#!/usr/bin/env bash
set -euo pipefail
# create isolated local clusters; never use an existing server.
: "${PGRX_PG_CONFIG_PATH:?set the PostgreSQL 18.6 pg_config path}"
case ${PIN_G0_TEST_HOOKS:-0} in
  0|1) mode=${PIN_G0_TEST_HOOKS:-0} ;;
  *) echo 'PIN_G0_TEST_HOOKS must be 0 or 1' >&2; exit 2 ;;
esac
if [[ $(id -u) == 0 ]]; then
  echo 'Run disposable PostgreSQL clusters as an unprivileged user.' >&2
  exit 2
fi
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin=$("$PGRX_PG_CONFIG_PATH" --bindir)
if [[ $("$PGRX_PG_CONFIG_PATH" --version) != 'PostgreSQL 18.6' ]]; then
  echo 'G0 smoke tests require PostgreSQL 18.6.' >&2
  exit 2
fi
work=$(mktemp -d /tmp/pin-g0.XXXXXXXX)
artifacts="$root/.artifacts/g0-$mode"
cleanup() {
  "$bin/pg_ctl" -D "$work/preload" -m immediate stop >/dev/null 2>&1 || true
  "$bin/pg_ctl" -D "$work/no-preload" -m immediate stop >/dev/null 2>&1 || true
  cp "$work"/*.log "$artifacts/" 2>/dev/null || true
  rm -rf -- "$work"
}
trap cleanup EXIT
mkdir -p "$artifacts" "$work/socket" "$work/no-preload-socket"
unset PGHOST PGHOSTADDR PGPORT PGDATABASE PGUSER PGSERVICE PGSERVICEFILE PGPASSFILE PGOPTIONS
port=55481
"$bin/initdb" -D "$work/preload" --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject >/dev/null
cat >> "$work/preload/postgresql.conf" <<CONF
shared_preload_libraries = 'pin'
listen_addresses = ''
unix_socket_directories = '$work/socket'
port = $port
fsync = on
full_page_writes = on
statement_timeout = '15s'
lock_timeout = '5s'
CONF
"$bin/pg_ctl" -D "$work/preload" -l "$work/preload.log" -w start
psql=("$bin/psql" -X -h "$work/socket" -p "$port" -d postgres -v ON_ERROR_STOP=1)
"${psql[@]}" -f "$root/tests/sql/smoke.sql" | tee "$work/smoke.log"
if [[ $mode == 1 ]]; then
  "${psql[@]}" -f "$root/tests/sql/errors.sql" | tee "$work/errors.log"
else
  "${psql[@]}" -f "$root/tests/sql/no_test_hooks.sql"
fi
"$bin/createdb" -h "$work/socket" -p "$port" --template=template0 --encoding=LATIN1 --locale=C pin_latin1
"$bin/psql" -X -h "$work/socket" -p "$port" -d pin_latin1 -v ON_ERROR_STOP=1 -f "$root/tests/sql/non_utf8.sql"
"$bin/pg_ctl" -D "$work/preload" -m fast -w restart -l "$work/preload.log"
"${psql[@]}" -c 'SELECT pin.abi_check(); DROP EXTENSION pin;'
"$bin/pg_ctl" -D "$work/preload" -m fast -w stop

"$bin/initdb" -D "$work/no-preload" --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject >/dev/null
cat >> "$work/no-preload/postgresql.conf" <<CONF
listen_addresses = ''
unix_socket_directories = '$work/no-preload-socket'
port = $port
statement_timeout = '15s'
lock_timeout = '5s'
CONF
"$bin/pg_ctl" -D "$work/no-preload" -l "$work/no-preload.log" -w start
# repeat in fresh backends as well as within each backend.
for attempt in 1 2; do
  "$bin/psql" -X -h "$work/no-preload-socket" -p "$port" -d postgres -v ON_ERROR_STOP=1 \
    -f "$root/tests/sql/no_preload.sql" | tee "$work/no-preload-$attempt.log"
done
"$bin/pg_ctl" -D "$work/no-preload" -m fast -w stop
