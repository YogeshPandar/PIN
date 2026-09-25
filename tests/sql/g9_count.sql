\set ON_ERROR_STOP on
SET search_path = public, pg_catalog;
SET max_parallel_workers_per_gather = 0;
SET max_parallel_maintenance_workers = 0;
SET pin.enable_grouped_storage = off;
SET pin.enable_grouped_scan = on;
SET pin.enable_count_fastpath = off;
SET pin.enable_grouped_count = off;
SET pin.enable_count_vm = off;

DO $$ BEGIN
    IF (SELECT boot_val FROM pg_settings WHERE name = 'pin.enable_grouped_count') IS DISTINCT FROM 'off' THEN
        RAISE EXCEPTION 'grouped count must default off';
    END IF;
END $$;

DROP TABLE IF EXISTS public.g9_count_docs;
CREATE TABLE public.g9_count_docs(id bigint PRIMARY KEY, body text, pad integer DEFAULT 0)
    WITH (fillfactor = 50, autovacuum_enabled = false);
INSERT INTO public.g9_count_docs(id, body)
SELECT i, 'common ' || CASE WHEN i % 2 = 0 THEN 'alpha ' ELSE 'delta ' END ||
    CASE WHEN i % 3 = 0 THEN 'beta ' ELSE 'gamma ' END ||
    CASE WHEN i <= 20 THEN 'rareplanet' ELSE '' END
FROM generate_series(1, 2400) AS i;
INSERT INTO public.g9_count_docs VALUES (-1, NULL, 0), (-2, '', 0);
CREATE INDEX g9_count_pin ON public.g9_count_docs USING pin(body);
CREATE INDEX g9_count_gin ON public.g9_count_docs USING gin(to_tsvector('simple', body));
ANALYZE public.g9_count_docs;

CREATE FUNCTION pg_temp.gc_check(source text, gin_source text, custom boolean DEFAULT true,
                                 certify boolean DEFAULT false)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    statement text := format('SELECT count(*) FROM ONLY public.g9_count_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', source);
    ids_sql text := format('SELECT coalesce(array_agg(id ORDER BY id), ARRAY[]::bigint[]) FROM ONLY public.g9_count_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', source);
    expected bigint[];
    indexed bigint[];
    control bigint[];
    actual bigint;
    plan jsonb;
    vm text;
BEGIN
    PERFORM set_config('pin.enable_count_fastpath', 'off', true);
    PERFORM set_config('enable_seqscan', 'on', true);
    PERFORM set_config('enable_bitmapscan', 'off', true);
    PERFORM set_config('enable_indexscan', 'off', true);
    PERFORM set_config('enable_indexonlyscan', 'off', true);
    EXECUTE ids_sql INTO expected;
    PERFORM set_config('enable_seqscan', 'off', true);
    PERFORM set_config('enable_bitmapscan', 'on', true);
    EXECUTE ids_sql INTO indexed;
    EXECUTE format('SELECT coalesce(array_agg(id ORDER BY id), ARRAY[]::bigint[]) FROM ONLY public.g9_count_docs WHERE to_tsvector(''simple'', body) @@ to_tsquery(''simple'', %L)', gin_source) INTO control;
    IF indexed IS DISTINCT FROM expected OR control IS DISTINCT FROM expected THEN
        RAISE EXCEPTION 'row identities differ for %: %, %, %', source, expected, indexed, control;
    END IF;
    PERFORM set_config('pin.enable_count_fastpath', 'on', true);
    PERFORM set_config('pin.enable_grouped_count', 'on', true);
    FOREACH vm IN ARRAY ARRAY['off', 'on'] LOOP
        PERFORM set_config('pin.enable_count_vm', vm, true);
        EXECUTE statement INTO actual;
        IF actual IS DISTINCT FROM cardinality(expected)::bigint THEN
            RAISE EXCEPTION 'count mismatch for %, VM %, got %, expected %', source, vm, actual, expected;
        END IF;
        EXECUTE 'EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) ' || statement INTO plan;
        IF custom IS NOT NULL AND
           (coalesce(plan #>> '{0,Plan,Custom Plan Provider}', '') = 'PinCount') IS DISTINCT FROM custom THEN
            RAISE EXCEPTION 'unexpected plan for %: %', source, plan;
        END IF;
        IF vm = 'off' AND coalesce((plan #>> '{0,Plan,Group VM Certified Roots}')::bigint, 0) <> 0 THEN
            RAISE EXCEPTION 'VM-disabled count certified roots: %', plan;
        END IF;
        IF certify AND vm = 'on' AND coalesce((plan #>> '{0,Plan,Group VM Certified Roots}')::bigint, 0) = 0 THEN
            RAISE EXCEPTION 'fixture never exercised grouped VM certification: %', plan;
        END IF;
        RAISE NOTICE 'group count plan % VM %: %', source, vm, plan;
    END LOOP;
END $$;

-- an old index without a grouped snapshot remains exact through heap fallback.
SELECT pg_temp.gc_check('alpha AND beta', 'alpha & beta');
SET pin.enable_grouped_storage = on;
SET maintenance_work_mem = '16MB';
VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0) public.g9_count_docs;
SELECT pg_temp.gc_check('common OR alpha', 'common | alpha', true, true);
SELECT pg_temp.gc_check('alpha AND beta', 'alpha & beta', true, true);
SELECT pg_temp.gc_check('alpha AND NOT beta', 'alpha & !beta', true, true);
SELECT pg_temp.gc_check('NOT alpha', '!alpha', true, true);
SELECT pg_temp.gc_check('NOT absent', '!absent', true, true);
SELECT pg_temp.gc_check('rareplanet', 'rareplanet', NULL);
SELECT pg_temp.gc_check('absent', 'absent', NULL);
SELECT pg_temp.gc_check('"alpha beta"', 'alpha <-> beta', false);
SELECT pg_temp.gc_check('alp*', 'alp:*', false);

-- indexed updates, HOT chains, NULL transitions, abort and both delta lengths.
SET pin.enable_grouped_storage = off;
BEGIN;
UPDATE public.g9_count_docs SET pad = pad + 1 WHERE id BETWEEN 1 AND 50;
DO $$ BEGIN
    IF coalesce((SELECT n_tup_hot_upd FROM pg_stat_xact_user_tables
                 WHERE relid = 'public.g9_count_docs'::regclass), 0) = 0 THEN
        RAISE EXCEPTION 'fixture did not produce a HOT update';
    END IF;
END $$;
COMMIT;
UPDATE public.g9_count_docs SET body = 'beta replacement' WHERE id = 60;
UPDATE public.g9_count_docs SET body = NULL WHERE id = 66;
DELETE FROM public.g9_count_docs WHERE id = 72;
INSERT INTO public.g9_count_docs VALUES (2401, 'alpha beta newterm', 0);
BEGIN;
INSERT INTO public.g9_count_docs VALUES (999999, 'alpha beta aborted', 0);
ROLLBACK;
SELECT pg_temp.gc_check('alpha AND beta', 'alpha & beta');
SELECT pg_temp.gc_check('NOT alpha', '!alpha');
INSERT INTO public.g9_count_docs
SELECT i, CASE WHEN i % 2 = 0 THEN 'alpha beta common' ELSE 'alpha delta common' END, 0
FROM generate_series(2402, 6500) AS i;
SELECT pg_temp.gc_check('alpha AND beta', 'alpha & beta');
SELECT pg_temp.gc_check('alpha OR beta', 'alpha | beta');
SET work_mem = '64kB';
SELECT pg_temp.gc_check('NOT absent', '!absent');
RESET work_mem;
VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0) public.g9_count_docs;
SELECT pg_temp.gc_check('alpha AND NOT beta', 'alpha & !beta');

-- the same cached Boolean plan must execute its real aggregate fallback after disable.
SET pin.enable_count_fastpath = on;
SET pin.enable_grouped_count = on;
SET pin.enable_count_vm = on;
SET enable_seqscan = off;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SET enable_bitmapscan = on;
SET plan_cache_mode = force_generic_plan;
PREPARE gc_cached AS SELECT count(*) FROM ONLY public.g9_count_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha OR beta');
EXECUTE gc_cached;
SET pin.enable_grouped_count = off;
DO $$ DECLARE plan jsonb; BEGIN
    EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) EXECUTE gc_cached' INTO plan;
    IF plan #>> '{0,Plan,Fallback}' IS DISTINCT FROM 'grouped count disabled at execution' THEN
        RAISE EXCEPTION 'cached plan did not use fallback: %', plan;
    END IF;
END $$;
DEALLOCATE gc_cached;
RESET plan_cache_mode;
SET pin.enable_grouped_count = on;
BEGIN ISOLATION LEVEL SERIALIZABLE;
DO $$ DECLARE plan jsonb; BEGIN
    EXECUTE $q$EXPLAIN (FORMAT JSON) SELECT count(*) FROM ONLY public.g9_count_docs
        WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha OR beta')$q$ INTO plan;
    IF plan::text LIKE '%PinCount%' THEN
        RAISE EXCEPTION 'serializable count must keep the core executor';
    END IF;
END $$;
ROLLBACK;

-- prove actual heap-slot reuse, not just equal row counts after DELETE/INSERT.
DROP TABLE IF EXISTS public.g9_count_reuse;
CREATE TABLE public.g9_count_reuse(id integer, body text) WITH (autovacuum_enabled = false);
INSERT INTO public.g9_count_reuse SELECT i, 'alpha' FROM generate_series(1, 2400) AS i;
SET pin.enable_grouped_storage = on;
CREATE INDEX g9_count_reuse_pin ON public.g9_count_reuse USING pin(body);
CREATE TEMP TABLE gc_old AS SELECT ctid AS old_tid FROM public.g9_count_reuse;
DELETE FROM public.g9_count_reuse;
SET pin.enable_grouped_storage = off;
VACUUM (INDEX_CLEANUP ON, TRUNCATE OFF) public.g9_count_reuse;
INSERT INTO public.g9_count_reuse SELECT i, 'beta' FROM generate_series(2401, 4800) AS i;
ANALYZE public.g9_count_reuse;
DO $$ DECLARE n bigint; plan jsonb; BEGIN
    IF NOT EXISTS (SELECT FROM public.g9_count_reuse r, gc_old o WHERE r.ctid = o.old_tid) THEN
        RAISE EXCEPTION 'fixture failed to reuse a heap slot';
    END IF;
    SELECT count(*) INTO n FROM ONLY public.g9_count_reuse
        WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha AND beta');
    IF n <> 0 THEN RAISE EXCEPTION 'old alpha and new beta created a false AND'; END IF;
    SELECT count(*) INTO n FROM ONLY public.g9_count_reuse
        WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha OR beta');
    IF n <> 2400 THEN RAISE EXCEPTION 'reused root was lost or counted twice'; END IF;
    EXECUTE $q$EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM ONLY public.g9_count_reuse
        WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha OR beta')$q$ INTO plan;
    IF plan #>> '{0,Plan,Custom Plan Provider}' IS DISTINCT FROM 'PinCount' THEN
        RAISE EXCEPTION 'reuse fixture did not exercise grouped count: %', plan;
    END IF;
END $$;
RESET ALL;
