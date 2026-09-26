#!/usr/bin/env bash
# bounded replay proof for packed postings and direct-TID build sealing.
set -euo pipefail
: "${PIN_PACKAGE:?set the native package directory}"
: "${PIN_OUTPUT:?set an empty output directory}"
test ! -e "$PIN_OUTPUT"
mkdir -p "$PIN_OUTPUT"
work=$(mktemp -d /tmp/pin-direct-recovery.XXXXXXXX)
mkdir -p "$work/socket"
bin=/usr/lib/postgresql/18/bin
package=$(realpath "$PIN_PACKAGE")
"$bin/initdb" -D "$work/data" --no-instructions --locale=C.UTF-8 > "$PIN_OUTPUT/initdb.log" 2>&1
cat >> "$work/data/postgresql.conf" <<CONF
shared_preload_libraries = '$package/usr/lib/postgresql/18/lib/pin'
extension_control_path = '$package/usr/share/postgresql/18:\$system'
dynamic_library_path = '$package/usr/lib/postgresql/18/lib:\$libdir'
unix_socket_directories = '$work/socket'
port = 55507
listen_addresses = ''
fsync = on
full_page_writes = on
autovacuum = off
CONF
export PGHOST="$work/socket" PGPORT=55507 PGDATABASE=postgres PGUSER="$(id -un)"
psql=("$bin/psql" -X -v ON_ERROR_STOP=1)
"$bin/pg_ctl" -D "$work/data" -l "$PIN_OUTPUT/server.log" -w start > "$PIN_OUTPUT/start.log"
actual=$("${psql[@]}" -Atqc 'SHOW data_directory')
test "$actual" = "$work/data"
"${psql[@]}" <<'SQL' > "$PIN_OUTPUT/setup.log"
CREATE EXTENSION pin;
SET pin.enable_direct_tid_build=on;
SET pin.enable_packed_postings=on;
CREATE TABLE public.pin_direct_crash(id integer PRIMARY KEY, body text)
    WITH (autovacuum_enabled=false);
INSERT INTO public.pin_direct_crash
SELECT i, CASE WHEN i % 2 = 0 THEN 'alpha beta' ELSE 'beta gamma' END
FROM generate_series(1,2000) i;
CREATE INDEX pin_direct_crash_idx ON public.pin_direct_crash USING pin(body);
DELETE FROM public.pin_direct_crash WHERE id % 7 = 0;
INSERT INTO public.pin_direct_crash VALUES
    (3000,'alpha beta'),(3001,'rareword'),(3002,'rareword');
SQL
python3 - "$work/transaction-started" > "$PIN_OUTPUT/inflight.log" 2>&1 <<'PY' &
import pathlib,sys,time,psycopg2
db=psycopg2.connect(dbname='postgres')
cur=db.cursor()
cur.execute("INSERT INTO public.pin_direct_crash VALUES (4000,'shouldnotcommit')")
pathlib.Path(sys.argv[1]).write_text('open')
time.sleep(30)
PY
client=$!
for _ in $(seq 1 100); do
    test -f "$work/transaction-started" && break
    sleep 0.05
done
test -f "$work/transaction-started"
"$bin/pg_ctl" -D "$work/data" -m immediate -w stop > "$PIN_OUTPUT/crash.log" 2>&1
kill "$client" 2>/dev/null || true
wait "$client" || true
"$bin/pg_ctl" -D "$work/data" -l "$PIN_OUTPUT/server.log" -w start > "$PIN_OUTPUT/restart.log"
"${psql[@]}" <<'SQL' > "$PIN_OUTPUT/verify.log"
SET pin.enable_direct_tid_build=off;
DO $$
DECLARE
    source text;
    expected integer[];
    actual integer[];
    statement text;
BEGIN
    PERFORM set_config('enable_indexscan','off',true);
    FOREACH source IN ARRAY ARRAY['alpha','beta','gamma','rareword',
                                  'alpha AND beta','shouldnotcommit'] LOOP
        statement := format('SELECT array_agg(id ORDER BY id) FROM public.pin_direct_crash WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', source);
        PERFORM set_config('enable_seqscan','on',true);
        PERFORM set_config('enable_bitmapscan','off',true);
        EXECUTE statement INTO expected;
        PERFORM set_config('enable_seqscan','off',true);
        PERFORM set_config('enable_bitmapscan','on',true);
        EXECUTE statement INTO actual;
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'direct-build replay mismatch for %', source;
        END IF;
    END LOOP;
    IF EXISTS (SELECT 1 FROM public.pin_direct_crash WHERE id=4000) THEN
        RAISE EXCEPTION 'uncommitted row survived replay';
    END IF;
END
$$;
INSERT INTO public.pin_direct_crash VALUES (5000,'newword'),(5001,'newword');
SELECT count(*) AS persisted_newword
FROM public.pin_direct_crash WHERE body OPERATOR(pin.@@@) pin.parse_query('newword');
CHECKPOINT;
SQL
index_path=$("${psql[@]}" -Atqc "SELECT pg_relation_filepath('public.pin_direct_crash_idx')")
python3 - "$work/data/$index_path" "$PIN_OUTPUT/result.json" <<'PY'
import json,pathlib,sys
raw=pathlib.Path(sys.argv[1]).read_bytes()
assert len(raw)%8192==0 and raw[24:28]==b'PIN2'
flags=int.from_bytes(raw[52:56],'little')
kinds={}
for i in range(0,len(raw),8192):
    kind=raw[i+30]
    kinds[str(kind)]=kinds.get(str(kind),0)+1
assert kinds.get('9',0)>0, kinds
pathlib.Path(sys.argv[2]).write_text(json.dumps(
    {'index_bytes':len(raw),'metapage_flags':flags,'page_kinds':kinds},indent=2)+'\n')
PY
"$bin/pg_ctl" -D "$work/data" -m fast -w stop > "$PIN_OUTPUT/shutdown.log"
printf 'direct-build WAL replay and indexed oracle passed\n'
