#!/usr/bin/env bash
set -euo pipefail

: "${PGRX_PG_CONFIG_PATH:?set the PostgreSQL 18.6 pg_config path}"
if [[ $(id -u) == 0 ]]; then
  echo 'Run Grouped COUNT qualification as an unprivileged user.' >&2
  exit 2
fi
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin=$("$PGRX_PG_CONFIG_PATH" --bindir)
version=$("$PGRX_PG_CONFIG_PATH" --version)
if [[ $version != 'PostgreSQL 18.6' && $version != 'PostgreSQL 18.6 '* ]]; then
  echo 'Grouped COUNT qualification requires PostgreSQL 18.6.' >&2
  exit 2
fi
mode=${PIN_G9_TEST_HOOKS:-0}
anchors=${PIN_G9_ANCHORS:-0}
owner_frontier=${PIN_G9_OWNER_FRONTIER:-0}
[[ $mode == 0 || $mode == 1 ]] || exit 2
[[ $anchors == 0 || $anchors == 1 ]] || exit 2
[[ $owner_frontier == 0 || $owner_frontier == 1 ]] || exit 2
work=$(mktemp -d /tmp/pin-count.XXXXXXXX)
artifacts="$root/.artifacts/grouped-count-$mode"
if [[ $anchors == 1 ]]; then artifacts+="-anchors"; fi
if [[ $owner_frontier == 1 ]]; then artifacts+="-owners"; fi
mkdir -p "$artifacts" "$work/socket"
cleanup() {
  "$bin/pg_ctl" -D "$work/data" -m immediate -w stop >/dev/null 2>&1 || true
  cp "$work/postgres.log" "$artifacts/postgres.log" 2>/dev/null || true
  rm -rf -- "$work"
}
trap cleanup EXIT
unset PGHOST PGHOSTADDR PGPORT PGDATABASE PGUSER PGSERVICE PGSERVICEFILE PGPASSFILE PGOPTIONS PGAPPNAME
"$bin/initdb" -D "$work/data" --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject >/dev/null
cat >> "$work/data/postgresql.conf" <<CONF
shared_preload_libraries = 'pin'
listen_addresses = ''
unix_socket_directories = '$work/socket'
port = 55490
fsync = on
full_page_writes = on
synchronous_commit = on
log_min_messages = debug1
log_temp_files = 0
statement_timeout = '180s'
lock_timeout = '60s'
autovacuum = off
CONF
"$bin/pg_ctl" -D "$work/data" -l "$work/postgres.log" -w start
export PGHOST="$work/socket" PGPORT=55490 PGDATABASE=postgres
python3 "$root/tools/g9_count_qualification.py" \
  --psql "$bin/psql" --pg-ctl "$bin/pg_ctl" --data "$work/data" \
  --server-log "$work/postgres.log" --artifacts "$artifacts" --hooks "$mode" \
  --anchors "$anchors" --owner-frontier "$owner_frontier"
