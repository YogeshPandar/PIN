#!/usr/bin/env bash
set -euo pipefail

pg_bin=$("${PGRX_PG_CONFIG_PATH:?set PostgreSQL 18 pg_config}" --bindir)
case ${PIN_DIRECT_FIRST:-0} in
  0|1) ;;
  *) echo 'PIN_DIRECT_FIRST must be 0 or 1' >&2; exit 2 ;;
esac
work=$(mktemp -d /tmp/pin-direct-grouped.XXXXXXXX)
mkdir -p "$work/socket"
cleanup() {
  "$pg_bin/pg_ctl" -D "$work/data" -m immediate -w stop >/dev/null 2>&1 || true
  rm -rf -- "$work"
}
trap cleanup EXIT

"$pg_bin/initdb" -D "$work/data" --encoding=UTF8 --locale=C --auth-local=trust --auth-host=reject >/dev/null
cat >> "$work/data/postgresql.conf" <<CONF
shared_preload_libraries = 'pin'
listen_addresses = ''
unix_socket_directories = '$work/socket'
port = 55493
autovacuum = off
CONF
"$pg_bin/pg_ctl" -D "$work/data" -l "$work/server.log" -w start >/dev/null
"$pg_bin/psql" -X -w -h "$work/socket" -p 55493 -d postgres -v ON_ERROR_STOP=1 -v direct_first="${PIN_DIRECT_FIRST:-0}" <<'SQL'
CREATE EXTENSION pin;
SET client_min_messages = debug1;
SET maintenance_work_mem = '128MB';
SET max_parallel_maintenance_workers = 0;
SET pin.enable_grouped_storage = on;
SET pin.enable_grouped_scan = on;
SET pin.enable_packed_postings = on;
CREATE TABLE direct_docs AS
SELECT n::bigint AS id,
       'shared item' || (n % 300)::text || ' ' ||
       CASE WHEN n % 2 = 0 THEN 'even' ELSE 'odd' END || ' tail' AS body
FROM generate_series(1, 12000) AS n;
CREATE TABLE legacy_docs AS SELECT * FROM direct_docs;
CREATE FUNCTION pg_temp.build_case(target text, direct boolean)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    started timestamptz;
    cpu bigint;
BEGIN
    PERFORM set_config('pin.enable_direct_grouped_build', direct::text, true);
    started := clock_timestamp();
    cpu := split_part(pg_read_file('/proc/self/schedstat'), ' ', 1)::bigint;
    EXECUTE format('CREATE INDEX %I ON %I USING pin(body)', target || '_pin', target);
    RAISE NOTICE '% build: cpu_ms=%, wall_ms=%', target,
        (split_part(pg_read_file('/proc/self/schedstat'), ' ', 1)::bigint - cpu) / 1000000.0,
        extract(epoch FROM clock_timestamp() - started) * 1000;
END;
$$;
\if :direct_first
SELECT pg_temp.build_case('direct_docs', true);
SELECT pg_temp.build_case('legacy_docs', false);
\else
SELECT pg_temp.build_case('legacy_docs', false);
SELECT pg_temp.build_case('direct_docs', true);
\endif
SET enable_seqscan = off;
DO $$
DECLARE
    query text;
    expected bigint[];
    actual bigint[];
BEGIN
    FOREACH query IN ARRAY ARRAY[
        'shared', 'item3', 'item3 AND even', 'item3 OR odd',
        'shared AND NOT even', 'missing', 'NOT missing',
        'item3*', '"shared item3"'
    ] LOOP
        EXECUTE format(
            'SELECT COALESCE(array_agg(id ORDER BY id), ARRAY[]::bigint[]) FROM legacy_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', query
        ) INTO expected;
        EXECUTE format(
            'SELECT COALESCE(array_agg(id ORDER BY id), ARRAY[]::bigint[]) FROM direct_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', query
        ) INTO actual;
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'direct build mismatch on %', query;
        END IF;
    END LOOP;
END;
$$;
SELECT pg_relation_size('legacy_docs_pin') AS legacy_bytes,
       pg_relation_size('direct_docs_pin') AS direct_bytes;
INSERT INTO legacy_docs VALUES (12001, 'shared item3 odd tail');
INSERT INTO direct_docs VALUES (12001, 'shared item3 odd tail');
UPDATE legacy_docs SET body = 'replacement even' WHERE id = 3;
UPDATE direct_docs SET body = 'replacement even' WHERE id = 3;
DELETE FROM legacy_docs WHERE id = 11;
DELETE FROM direct_docs WHERE id = 11;
VACUUM legacy_docs;
VACUUM direct_docs;
DO $$
DECLARE
    query text;
    expected bigint[];
    actual bigint[];
BEGIN
    FOREACH query IN ARRAY ARRAY['shared', 'item3 AND odd', 'replacement', 'NOT even'] LOOP
        EXECUTE format(
            'SELECT COALESCE(array_agg(id ORDER BY id), ARRAY[]::bigint[]) FROM legacy_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', query
        ) INTO expected;
        EXECUTE format(
            'SELECT COALESCE(array_agg(id ORDER BY id), ARRAY[]::bigint[]) FROM direct_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', query
        ) INTO actual;
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'direct build lifecycle mismatch on %', query;
        END IF;
    END LOOP;
END;
$$;
SQL
