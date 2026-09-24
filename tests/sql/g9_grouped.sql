\set ON_ERROR_STOP on
\ir g2_helpers.sql

SET client_min_messages = debug1;
SET max_parallel_maintenance_workers = 0;
SET max_parallel_workers_per_gather = 0;
SET pin.enable_count_fastpath = off;
SET pin.enable_count_vm = off;

DO $$
BEGIN
    IF current_setting('pin.enable_grouped_storage') <> 'off'
       OR current_setting('pin.enable_grouped_scan') <> 'off' THEN
        RAISE EXCEPTION 'grouped storage and scans must start disabled';
    END IF;
END;
$$;

CREATE FUNCTION pg_temp.g9_compare(target regclass, source text)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    expected bigint[];
    actual bigint[];
BEGIN
    expected := pg_temp.g2_result(target, source, false);
    actual := pg_temp.g2_result(target, source, true);
    IF actual IS DISTINCT FROM expected THEN
        RAISE EXCEPTION 'grouped mismatch on % for %: expected %, actual %',
            target, source, expected, actual;
    END IF;
END;
$$;

CREATE TABLE public.g9_docs(id bigint, body text NOT NULL, padding text)
    WITH (autovacuum_enabled = false);
ALTER TABLE public.g9_docs ALTER COLUMN padding SET STORAGE PLAIN;
INSERT INTO public.g9_docs
SELECT n,
       'common ' || CASE WHEN n % 2 = 0 THEN 'alpha ' ELSE 'delta ' END
                 || CASE WHEN n % 3 = 0 THEN 'beta' ELSE 'gamma' END,
       repeat('p', 1800)
FROM generate_series(1, 12000) AS n;
CREATE INDEX g9_docs_pin ON public.g9_docs USING pin(body);
ANALYZE public.g9_docs;

-- a legacy index remains readable with the new scan gate enabled.
SET pin.enable_grouped_scan = on;
SELECT pg_temp.g9_compare('g9_docs', 'alpha AND beta');

-- insufficient maintenance memory must skip publication, not lose rows.
SET pin.enable_grouped_storage = on;
SET maintenance_work_mem = '1MB';
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) public.g9_docs;
SELECT pg_temp.g9_compare('g9_docs', 'alpha AND NOT beta');

-- the fixture exceeds both one page group and the remaining tuplesort budget.
SET maintenance_work_mem = '4MB';
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) public.g9_docs;
DO $$
DECLARE
    source text;
    expected bigint[];
BEGIN
    IF pg_relation_size('g9_docs') < 256 * 8192 THEN
        RAISE EXCEPTION 'grouped fixture did not span 256 heap pages';
    END IF;
    FOREACH source IN ARRAY ARRAY[
        'alpha', 'common', 'missing', 'alpha AND beta', 'alpha AND NOT beta',
        'alpha OR beta', '(alpha AND beta) OR gamma',
        'alpha AND (beta OR gamma)', 'NOT alpha', 'NOT (alpha OR beta)',
        'NOT missing', 'alpha OR NOT alpha', 'alpha AND NOT alpha',
        'alpha*', '"alpha beta"', 'NOT "alpha beta"'
    ] LOOP
        PERFORM pg_temp.g9_compare('g9_docs', source);
    END LOOP;
    SELECT array_agg(n::bigint ORDER BY n) INTO expected
    FROM generate_series(6, 12000, 6) AS n;
    PERFORM pg_temp.g2_expect('g9_docs', 'alpha AND beta', expected);
END;
$$;

-- bitmap lossification and forced predicate rechecks preserve all heap matches.
SET work_mem = '64kB';
SELECT pg_temp.g9_compare('g9_docs', 'common');
SET pin.enable_exact_bitmap = off;
SELECT pg_temp.g9_compare('g9_docs', 'alpha AND NOT beta');
SET pin.enable_exact_bitmap = on;
DO $$
DECLARE plan json;
BEGIN
    SET LOCAL enable_seqscan = off;
    SET LOCAL enable_indexscan = off;
    SET LOCAL enable_indexonlyscan = off;
    SET LOCAL enable_bitmapscan = on;
    EXECUTE $q$EXPLAIN (ANALYZE, FORMAT JSON, TIMING OFF)
        SELECT id FROM g9_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('common')$q$
        INTO plan;
    IF plan::jsonb #>> '{0,Plan,Node Type}' <> 'Bitmap Heap Scan'
       OR COALESCE((plan::jsonb #>> '{0,Plan,Lossy Heap Blocks}')::integer, 0) = 0 THEN
        RAISE EXCEPTION 'grouped fixture did not exercise a lossy bitmap: %', plan;
    END IF;
END;
$$;
RESET work_mem;

-- unsealed writes are visible immediately; aborted writes remain invisible.
SET pin.enable_grouped_storage = off;
INSERT INTO g9_docs VALUES (12001, 'alpha beta newterm', repeat('q', 1800));
BEGIN;
INSERT INTO g9_docs VALUES (12002, 'alpha beta aborted', repeat('q', 1800));
ROLLBACK;
SELECT pg_temp.g2_expect('g9_docs', 'newterm', ARRAY[12001]::bigint[]);
SELECT pg_temp.g2_expect('g9_docs', 'aborted', ARRAY[]::bigint[]);
UPDATE g9_docs SET padding = repeat('h', 1800) WHERE id = 6;
UPDATE g9_docs SET body = 'alpha changed' WHERE id = 7;
SELECT pg_temp.g9_compare('g9_docs', 'alpha AND beta');
SELECT pg_temp.g2_expect('g9_docs', 'changed', ARRAY[7]::bigint[]);

-- multiple scan keys must retain the executor's full predicate recheck.
DO $$
DECLARE expected bigint[]; actual bigint[]; statement text;
BEGIN
    statement := $q$SELECT array_agg(id ORDER BY id) FROM g9_docs
        WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')
          AND body OPERATOR(pin.@@@) pin.parse_query('beta')$q$;
    SET LOCAL enable_bitmapscan = off;
    SET LOCAL enable_indexscan = off;
    SET LOCAL enable_indexonlyscan = off;
    SET LOCAL enable_seqscan = on;
    EXECUTE statement INTO expected;
    SET LOCAL enable_seqscan = off;
    SET LOCAL enable_bitmapscan = on;
    EXECUTE statement INTO actual;
    IF actual IS DISTINCT FROM expected THEN
        RAISE EXCEPTION 'grouped multiple-key recheck lost matches';
    END IF;
END;
$$;

-- clearing liveness cannot depend on either experimental gate.
SET pin.enable_grouped_scan = off;
DELETE FROM g9_docs WHERE id % 5 = 0;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_docs;
SET pin.enable_grouped_scan = on;
SELECT pg_temp.g9_compare('g9_docs', 'alpha AND beta');
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_docs;
SELECT pg_temp.g9_compare('g9_docs', 'NOT alpha');

-- create-index, empty snapshots, hot roots and reused slots use the same format.
CREATE TABLE g9_small(id bigint, body text, marker integer)
    WITH (autovacuum_enabled = false, fillfactor = 50);
CREATE INDEX g9_small_pin ON g9_small USING pin(body);
INSERT INTO g9_small VALUES (1, 'alpha', 0), (2, 'alpha beta', 0);
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_small;
UPDATE g9_small SET marker = marker + 1 WHERE id = 2;
SELECT pg_temp.g2_expect('g9_small', 'alpha AND beta', ARRAY[2]::bigint[]);
CREATE TEMP TABLE g9_old_tid AS SELECT ctid AS old_tid FROM g9_small WHERE id = 1;
DELETE FROM g9_small WHERE id = 1;
SET pin.enable_grouped_storage = off;
SET pin.enable_grouped_scan = off;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_small;
INSERT INTO g9_small VALUES (3, 'beta', 0);
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM g9_small n, g9_old_tid o
                   WHERE n.id = 3 AND n.ctid = o.old_tid) THEN
        RAISE EXCEPTION 'grouped reuse fixture did not reuse the retired heap slot';
    END IF;
END;
$$;
SET pin.enable_grouped_scan = on;
SELECT pg_temp.g2_expect('g9_small', 'alpha AND beta', ARRAY[2]::bigint[]);
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_small;
SELECT pg_temp.g2_expect('g9_small', 'alpha AND beta', ARRAY[2]::bigint[]);
REINDEX INDEX g9_small_pin;
SELECT pg_temp.g2_expect('g9_small', 'beta', ARRAY[2, 3]::bigint[]);

-- reindex with storage disabled restores the legacy-only physical representation.
SET pin.enable_grouped_storage = off;
REINDEX INDEX g9_small_pin;
SELECT pg_temp.g2_expect('g9_small', 'alpha AND beta', ARRAY[2]::bigint[]);
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_small;
SELECT pg_temp.g2_expect('g9_small', 'beta', ARRAY[2, 3]::bigint[]);

\ir issue14_recheck.sql
