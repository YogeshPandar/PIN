#!/usr/bin/env bash
set -euo pipefail

: "${PGRX_PG_CONFIG_PATH:?set the PostgreSQL 18.6 pg_config path}"
if [[ $(id -u) == 0 ]]; then
  echo 'Run G9 qualification as an unprivileged user.' >&2
  exit 2
fi
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin=$("$PGRX_PG_CONFIG_PATH" --bindir)
version=$("$PGRX_PG_CONFIG_PATH" --version)
if [[ $version != 'PostgreSQL 18.6' && $version != 'PostgreSQL 18.6 '* ]]; then
  echo 'G9 qualification requires PostgreSQL 18.6.' >&2
  exit 2
fi
mode=${PIN_G9_TEST_HOOKS:-0}
[[ $mode == 0 || $mode == 1 ]] || exit 2
work=$(mktemp -d /tmp/pin-g9.XXXXXXXX)
artifacts="$root/.artifacts/g9-$mode"
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
port = 55489
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
export PGHOST="$work/socket" PGPORT=55489 PGDATABASE=postgres
python3 "$root/tools/g9_qualification.py" \
  --psql "$bin/psql" --pg-ctl "$bin/pg_ctl" --data "$work/data" \
  --server-log "$work/postgres.log" --artifacts "$artifacts" --hooks "$mode"

if [[ $mode == 0 ]]; then
  "$bin/psql" -X -w -v ON_ERROR_STOP=1 -f "$root/tests/sql/g9_profile.sql"
  python3 "$root/tools/g9_profile.py" --bindir "$bin" \
    --output "$artifacts/profile" --samples 6 --queries 2 --warmup 1 \
    --backend-proc /proc --host-note 'CI smoke only; not isolated performance evidence'
fi
