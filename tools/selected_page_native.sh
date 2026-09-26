#!/usr/bin/env bash
# compare exact main and selected-page packages on fresh pg18 clusters.
set -euo pipefail
: "${PIN_BASE_PACKAGE:?set baseline package directory}"
: "${PIN_SELECTED_PACKAGE:?set selected package directory}"
: "${PIN_OUTPUT:?set an empty output directory}"
test ! -e "$PIN_OUTPUT"
mkdir -p "$PIN_OUTPUT"
bin=/usr/lib/postgresql/18/bin
work=$(mktemp -d /tmp/pin-selected-native.XXXXXXXX)
active=""
cleanup() {
    if test -n "$active"; then
        "$bin/pg_ctl" -D "$active" -m fast -w stop > /dev/null 2>&1 || true
    fi
}
trap cleanup EXIT
for mode in baseline selected; do
    if test "$mode" = baseline; then
        package=$(realpath "$PIN_BASE_PACKAGE")
    else
        package=$(realpath "$PIN_SELECTED_PACKAGE")
    fi
    data="$work/$mode/data"
    socket="$work/$mode/socket"
    mkdir -p "$socket"
    "$bin/initdb" -D "$data" --no-instructions --locale=C.UTF-8 > "$PIN_OUTPUT/$mode-initdb.log" 2>&1
    cat >> "$data/postgresql.conf" <<CONF
shared_preload_libraries = '$package/usr/lib/postgresql/18/lib/pin'
extension_control_path = '$package/usr/share/postgresql/18:\$system'
dynamic_library_path = '$package/usr/lib/postgresql/18/lib:\$libdir'
unix_socket_directories = '$socket'
port = 55512
listen_addresses = ''
fsync = on
full_page_writes = on
autovacuum = off
shared_buffers = '128MB'
CONF
    active="$data"
    "$bin/pg_ctl" -D "$data" -l "$PIN_OUTPUT/$mode-server.log" -w start > "$PIN_OUTPUT/$mode-start.log"
    export PGHOST="$socket" PGPORT=55512 PGDATABASE=postgres PGUSER="$(id -un)"
    "$bin/psql" -X -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION pin' > "$PIN_OUTPUT/$mode-extension.log"
    python3 "$(dirname "$0")/selected_page_bench.py" --disposable --output "$PIN_OUTPUT/$mode" \
        > "$PIN_OUTPUT/$mode-benchmark.log" 2>&1
    "$bin/pg_ctl" -D "$data" -m fast -w stop > "$PIN_OUTPUT/$mode-stop.log"
    active=""
done
