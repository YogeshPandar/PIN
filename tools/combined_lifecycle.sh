#!/usr/bin/env bash
# native phrase, heap lifecycle, and visibility checks with both creation modes.
set -euo pipefail
: "${PIN_PACKAGE:?set native package directory}"
: "${PIN_OUTPUT:?set empty output directory}"
test ! -e "$PIN_OUTPUT"
mkdir -p "$PIN_OUTPUT"
package=$(realpath "$PIN_PACKAGE")
work=$(mktemp -d /tmp/pin-packed-direct-life.XXXXXXXX)
bin=/usr/lib/postgresql/18/bin
mkdir -p "$work/socket"
"$bin/initdb" -D "$work/data" --no-instructions --locale=C.UTF-8 > "$PIN_OUTPUT/initdb.log" 2>&1
cat >> "$work/data/postgresql.conf" <<CONF
shared_preload_libraries = '$package/usr/lib/postgresql/18/lib/pin'
extension_control_path = '$package/usr/share/postgresql/18:\$system'
dynamic_library_path = '$package/usr/lib/postgresql/18/lib:\$libdir'
unix_socket_directories = '$work/socket'
port = 55514
listen_addresses = ''
fsync = on
full_page_writes = on
autovacuum = off
CONF
active=1
cleanup() {
    if test "$active" = 1; then
        "$bin/pg_ctl" -D "$work/data" -m fast -w stop > /dev/null 2>&1 || true
    fi
}
trap cleanup EXIT
"$bin/pg_ctl" -D "$work/data" -l "$PIN_OUTPUT/server.log" -w start > "$PIN_OUTPUT/start.log"
export PGHOST="$work/socket" PGPORT=55514 PGDATABASE=postgres PGUSER="$(id -un)"
"$bin/psql" -X -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION pin' > "$PIN_OUTPUT/extension.log"
python3 "$(dirname "$0")/phrase_lifecycle.py" --direct-build --packed \
    --output "$PIN_OUTPUT/lifecycle" > "$PIN_OUTPUT/lifecycle.log" 2>&1
"$bin/pg_ctl" -D "$work/data" -m fast -w stop > "$PIN_OUTPUT/stop.log"
active=0
