\set ON_ERROR_STOP on
SET search_path = pg_catalog;
SET max_parallel_workers_per_gather = 0;

DO $$
BEGIN
    IF current_setting('pin.enable_count_recheck') <> 'off' THEN
        RAISE EXCEPTION 'G6 rechecks must start disabled in this fresh session';
    END IF;
END
$$;

CREATE TABLE public.g6_docs(id bigint PRIMARY KEY, body text, pad integer DEFAULT 0)
    WITH (fillfactor = 65, autovacuum_enabled = false);
INSERT INTO public.g6_docs
SELECT i, CASE i % 8
    WHEN 0 THEN 'alpha beta can''t 32.3'
    WHEN 1 THEN 'ALPHA BETA a_b a:b a.b'
    WHEN 2 THEN 'CAFÉ cafe' || chr(769) || ' Σ ς K'
    WHEN 3 THEN 'Straße STRASSE ẞ ß'
    WHEN 4 THEN ''
    WHEN 5 THEN NULL
    WHEN 6 THEN 'alpha ' || repeat('gamma ', 400)
    ELSE 'beta absent_from_alpha' END, 0
FROM generate_series(1, 4096) AS i;
CREATE INDEX g6_docs_pin ON public.g6_docs USING pin(body pin.text_ops);
ANALYZE public.g6_docs;

CREATE FUNCTION pg_temp.g6_expect(source text, custom boolean DEFAULT true)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    statement text := format('SELECT count(*) FROM ONLY public.g6_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', source);
    baseline bigint;
    actual bigint;
    mode text;
    root jsonb;
    plan jsonb;
BEGIN
    PERFORM set_config('pin.enable_count_fastpath', 'off', true);
    PERFORM set_config('enable_seqscan', 'on', true);
    PERFORM set_config('enable_bitmapscan', 'off', true);
    PERFORM set_config('enable_indexscan', 'off', true);
    PERFORM set_config('enable_indexonlyscan', 'off', true);
    EXECUTE statement INTO baseline;
    PERFORM set_config('enable_seqscan', 'off', true);
    PERFORM set_config('enable_bitmapscan', 'on', true);
    PERFORM set_config('pin.enable_count_fastpath', 'on', true);
    PERFORM set_config('pin.enable_count_vm', 'off', true);
    FOREACH mode IN ARRAY ARRAY['off', 'on', 'off'] LOOP
        PERFORM set_config('pin.enable_count_recheck', mode, true);
        EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) ' || statement INTO plan;
        root := plan->0->'Plan';
        IF (COALESCE(root->>'Custom Plan Provider', '') = 'PinCount') IS DISTINCT FROM custom THEN
            RAISE EXCEPTION 'G6 unexpected plan for %, mode %: %', source, mode, plan;
        END IF;
        IF custom THEN
            IF (root->>'VM Certified Roots')::bigint IS DISTINCT FROM 0 THEN
                RAISE EXCEPTION 'G6 certification mismatch: %', plan;
            END IF;
            IF baseline > 0 AND COALESCE((root->>'Heap Fetches')::bigint, 0) = 0 THEN
                RAISE EXCEPTION 'G6 recheck fixture did not fetch the heap: %', plan;
            END IF;
        END IF;
        EXECUTE statement INTO actual;
        IF actual IS DISTINCT FROM baseline THEN
            RAISE EXCEPTION 'G6 mismatch for %, mode %, expected %, got %', source, mode, baseline, actual;
        END IF;
    END LOOP;
END
$$;

SELECT pg_temp.g6_expect(source) FROM unnest(ARRAY[
    'alpha', 'beta', 'café', 'σ', 'k', 'straße', 'strasse', 'ß', 'can''t', '32.3', 'neverpresent'
]) AS source;
SELECT pg_temp.g6_expect('alpha OR beta', false);
SELECT pg_temp.g6_expect('"alpha beta"', false);
SELECT pg_temp.g6_expect('alp*', false);
SELECT pg_temp.g6_expect('NOT alpha', false);
BEGIN ISOLATION LEVEL SERIALIZABLE;
SELECT pg_temp.g6_expect('alpha', false);
COMMIT;
ALTER TABLE public.g6_docs ENABLE ROW LEVEL SECURITY;
SELECT pg_temp.g6_expect('alpha', false);
ALTER TABLE public.g6_docs DISABLE ROW LEVEL SECURITY;
VACUUM (INDEX_CLEANUP ON) public.g6_docs;
SELECT pg_temp.g6_expect('alpha');

-- HOT, changed indexed values, own writes and savepoint rollback keep the oracle.
UPDATE public.g6_docs SET pad = pad + 1 WHERE id % 7 = 0;
UPDATE public.g6_docs SET body = 'beta' WHERE id = 8;
DELETE FROM public.g6_docs WHERE id = 16;
SELECT pg_temp.g6_expect('alpha');
BEGIN;
SAVEPOINT g6_write;
INSERT INTO public.g6_docs VALUES (5000, 'ALPHA café', 0);
SELECT pg_temp.g6_expect('alpha');
ROLLBACK TO g6_write;
SELECT pg_temp.g6_expect('alpha');
COMMIT;

-- mode selection is refreshed for execution of an already cached plan.
SET enable_seqscan = off;
SET enable_bitmapscan = on;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SET pin.enable_count_fastpath = on;
SET pin.enable_count_vm = off;
SET plan_cache_mode = force_generic_plan;
PREPARE g6_cached AS SELECT count(*) FROM ONLY public.g6_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha');
DO $$
DECLARE
    mode text;
    plan jsonb;
    actual bigint;
BEGIN
    FOREACH mode IN ARRAY ARRAY['off', 'on', 'off', 'on'] LOOP
        PERFORM set_config('pin.enable_count_recheck', mode, true);
        EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) EXECUTE g6_cached' INTO plan;
        IF current_setting('pin.enable_count_recheck') IS DISTINCT FROM mode OR
           plan->0->'Plan'->>'Custom Plan Provider' IS DISTINCT FROM 'PinCount' THEN
            RAISE EXCEPTION 'G6 stale prepared recheck setting or plan: %', plan;
        END IF;
        EXECUTE 'EXECUTE g6_cached' INTO actual;
        IF actual <> 1534 THEN
            RAISE EXCEPTION 'G6 prepared result mismatch: %', actual;
        END IF;
    END LOOP;
END
$$;
DEALLOCATE g6_cached;
RESET plan_cache_mode;
RESET pin.enable_count_fastpath;
RESET pin.enable_count_vm;
RESET pin.enable_count_recheck;
RESET enable_seqscan;
RESET enable_bitmapscan;
RESET enable_indexscan;
RESET enable_indexonlyscan;
RESET max_parallel_workers_per_gather;
DROP TABLE public.g6_docs;

-- an unprivileged role cannot turn on the experimental recheck setting.
CREATE ROLE pin_g6_setting_check;
SET ROLE pin_g6_setting_check;
DO $$
BEGIN
    BEGIN
        PERFORM set_config('pin.enable_count_recheck', 'on', false);
        RAISE EXCEPTION 'G6 recheck setting was not restricted';
    EXCEPTION WHEN insufficient_privilege THEN
        NULL;
    END;
END
$$;
RESET ROLE;
DROP ROLE pin_g6_setting_check;
