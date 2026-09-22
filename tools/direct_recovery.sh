#!/usr/bin/env bash
# exercise direct-page WAL replay and heap-slot reuse in a disposable PG18 cluster.
set -euo pipefail
: "${PGRX_PG_CONFIG_PATH:?set the pinned PostgreSQL 18 pg_config path}"
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin=$("$PGRX_PG_CONFIG_PATH" --bindir)
work=$(mktemp -d /tmp/pin-direct-recovery.XXXXXXXX)
mkdir -p "$work/socket" "$root/.artifacts/direct-recovery"
cleanup() {
    status=$?
    "$bin/pg_ctl" -D "$work/data" -m immediate stop >/dev/null 2>&1 || true
    cp "$work"/*.log "$root/.artifacts/direct-recovery/" 2>/dev/null || true
    rm -rf -- "$work"
    exit "$status"
}
trap cleanup EXIT
"$bin/initdb" -D "$work/data" --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject > "$work/initdb.log"
cat >> "$work/data/postgresql.conf" <<CONF
shared_preload_libraries = 'pin'
listen_addresses = ''
unix_socket_directories = '$work/socket'
port = 55484
fsync = on
full_page_writes = on
synchronous_commit = on
checkpoint_timeout = '1h'
CONF
export PGHOST="$work/socket" PGPORT=55484 PGDATABASE=postgres PGUSER="$(id -un)"
export PGOPTIONS='-c pin.enable_direct_tid_segments=on -c pin.enable_count_fastpath=off'
psql=("$bin/psql" -X -v ON_ERROR_STOP=1)
"$bin/pg_ctl" -D "$work/data" -l "$work/postgres.log" -w start >/dev/null
"${psql[@]}" <<'SQL' > "$work/setup.log"
CREATE EXTENSION pin;
CREATE TABLE public.direct_crash (id integer, body text)
    WITH (fillfactor = 70, autovacuum_enabled = false);
INSERT INTO public.direct_crash
SELECT i, CASE WHEN i % 2 = 0 THEN 'alpha beta' ELSE 'beta gamma' END
FROM generate_series(1, 2000) AS i;
CREATE INDEX direct_crash_pin ON public.direct_crash USING pin(body);
VACUUM (PARALLEL 0, INDEX_CLEANUP ON) public.direct_crash;
DELETE FROM public.direct_crash WHERE id % 2 = 0;
VACUUM (PARALLEL 0, INDEX_CLEANUP ON) public.direct_crash;
INSERT INTO public.direct_crash
SELECT 10000 + i, 'replacement' FROM generate_series(1, 1000) AS i;
SQL
"$bin/pg_ctl" -D "$work/data" -m immediate -w stop > "$work/crash.log" 2>&1
"$bin/pg_ctl" -D "$work/data" -l "$work/postgres.log" -w start > "$work/restart.log" 2>&1
"${psql[@]}" <<'SQL' > "$work/verify.log"
DO $$
DECLARE
    source text;
    expected integer[];
    actual integer[];
    statement text;
BEGIN
    FOREACH source IN ARRAY ARRAY['alpha', 'beta', 'gamma', 'replacement',
                                  'alpha OR replacement', 'beta AND gamma'] LOOP
        statement := format('SELECT array_agg(id ORDER BY id) FROM public.direct_crash WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', source);
        PERFORM set_config('enable_seqscan', 'on', true);
        PERFORM set_config('enable_bitmapscan', 'off', true);
        PERFORM set_config('enable_indexscan', 'off', true);
        EXECUTE statement INTO expected;
        PERFORM set_config('enable_seqscan', 'off', true);
        PERFORM set_config('enable_bitmapscan', 'on', true);
        EXECUTE statement INTO actual;
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'direct WAL replay changed %', source;
        END IF;
    END LOOP;
END
$$;
SELECT count(*) FILTER (WHERE body = 'replacement') AS replacement_rows,
       count(*) FILTER (WHERE body LIKE '%alpha%') AS stale_alpha_rows
FROM public.direct_crash;
CHECKPOINT;
SQL
relpath=$("${psql[@]}" -Atqc "SELECT pg_relation_filepath('public.direct_crash_pin')")
python3 - "$work/data/$relpath" <<'PY' > "$work/pages.log"
import collections
import pathlib
import sys
raw = pathlib.Path(sys.argv[1]).read_bytes()
assert len(raw) % 8192 == 0
kinds = collections.Counter(raw[block + 30] for block in range(0, len(raw), 8192))
assert kinds[9] > 0, kinds
print(f'direct_pages={kinds[9]} total_pages={len(raw) // 8192}')
PY
"${psql[@]}" -c 'DROP TABLE public.direct_crash' > "$work/cleanup.log"
printf 'direct WAL recovery and indexed identity checks passed\n'
