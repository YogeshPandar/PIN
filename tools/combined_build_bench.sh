#!/usr/bin/env bash
# qualify packed dictionary and direct-tid build settings in one pg18 binary.
set -euo pipefail
: "${PIN_PACKAGE:?set native package directory}"
: "${PIN_OUTPUT:?set empty output directory}"
pin_rows=${PIN_ROWS:-16000}
pin_terms=${PIN_TERMS:-2000}
layouts=(plain grouped)
packing_order=(unpacked packed)
if test "${PIN_GROUPED_ONLY:-0}" = 1; then
    layouts=(grouped)
fi
if test "${PIN_REVERSE_PACKING:-0}" = 1; then
    packing_order=(packed unpacked)
fi
test ! -e "$PIN_OUTPUT"
mkdir -p "$PIN_OUTPUT"
package=$(realpath "$PIN_PACKAGE")
work=$(mktemp -d /tmp/pin-packed-direct.XXXXXXXX)
bin=/usr/lib/postgresql/18/bin
mkdir -p "$work/socket"
"$bin/initdb" -D "$work/data" --no-instructions --locale=C.UTF-8 > "$PIN_OUTPUT/initdb.log" 2>&1
cat >> "$work/data/postgresql.conf" <<CONF
shared_preload_libraries = '$package/usr/lib/postgresql/18/lib/pin'
extension_control_path = '$package/usr/share/postgresql/18:\$system'
dynamic_library_path = '$package/usr/lib/postgresql/18/lib:\$libdir'
unix_socket_directories = '$work/socket'
port = 55513
listen_addresses = ''
fsync = on
full_page_writes = on
autovacuum = off
shared_buffers = '128MB'
CONF
active=1
cleanup() {
    if test "$active" = 1; then
        "$bin/pg_ctl" -D "$work/data" -m fast -w stop > /dev/null 2>&1 || true
    fi
}
trap cleanup EXIT
"$bin/pg_ctl" -D "$work/data" -l "$PIN_OUTPUT/server.log" -w start > "$PIN_OUTPUT/start.log"
export PGHOST="$work/socket" PGPORT=55513 PGDATABASE=postgres PGUSER="$(id -un)"
"$bin/psql" -X -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION pin' > "$PIN_OUTPUT/extension.log"
for layout in "${layouts[@]}"; do
    for packing in "${packing_order[@]}"; do
        args=()
        test "$layout" = grouped && args+=(--grouped)
        test "$packing" = packed && args+=(--packed)
        python3 "$(dirname "$0")/direct_build_bench.py" --disposable \
            --output "$PIN_OUTPUT/$layout-$packing" --rows "$pin_rows" \
            --terms "$pin_terms" "${args[@]}" \
            > "$PIN_OUTPUT/$layout-$packing.log" 2>&1
    done
done
"$bin/pg_ctl" -D "$work/data" -m fast -w stop > "$PIN_OUTPUT/stop.log"
active=0
