\set ON_ERROR_STOP on
SET search_path = pg_catalog;
SET max_parallel_workers_per_gather = 0;
SET pin.enable_count_fastpath = off;
SET track_functions = 'all';
CREATE TABLE public.exact_bitmap_docs(id integer PRIMARY KEY, body text, pad text)
    WITH (fillfactor = 70, autovacuum_enabled = false);
INSERT INTO public.exact_bitmap_docs
SELECT i, CASE i % 8
    WHEN 0 THEN 'alpha beta' WHEN 1 THEN 'beta alpha'
    WHEN 2 THEN 'ALPHA café' WHEN 3 THEN 'alpha alpha'
    WHEN 4 THEN 'beta' WHEN 5 THEN 'alphabet'
    WHEN 6 THEN '' ELSE NULL END, repeat('x', 1000)
FROM generate_series(1, 16000) i;
CREATE INDEX exact_bitmap_pin ON public.exact_bitmap_docs USING pin(body);
ANALYZE public.exact_bitmap_docs;
CREATE FUNCTION pg_temp.check_exact_bitmap(predicate text, exact boolean,
                                         memory text DEFAULT '64MB')
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    statement text := 'SELECT array_agg(id ORDER BY id) FROM public.exact_bitmap_docs WHERE ' || predicate;
    expected integer[];
    actual integer[];
    before_calls bigint;
    after_calls bigint;
    plan jsonb;
BEGIN
    PERFORM set_config('work_mem', memory, true);
    PERFORM set_config('enable_seqscan', 'on', true);
    PERFORM set_config('enable_bitmapscan', 'off', true);
    PERFORM set_config('enable_indexscan', 'off', true);
    EXECUTE statement INTO expected;
    SELECT COALESCE(sum(calls), 0) INTO before_calls FROM pg_stat_xact_user_functions
      WHERE funcid = 'pin.matches(text,pin.query)'::regprocedure;
    PERFORM set_config('enable_seqscan', 'off', true);
    PERFORM set_config('enable_bitmapscan', 'on', true);
    EXECUTE statement INTO actual;
    SELECT COALESCE(sum(calls), 0) INTO after_calls FROM pg_stat_xact_user_functions
      WHERE funcid = 'pin.matches(text,pin.query)'::regprocedure;
    IF actual IS DISTINCT FROM expected THEN
        RAISE EXCEPTION 'indexed identity mismatch: %', predicate;
    END IF;
    IF exact AND after_calls <> before_calls THEN
        RAISE EXCEPTION 'exact bitmap unnecessarily reevaluated predicate: %', predicate;
    ELSIF NOT exact AND after_calls <= before_calls THEN
        RAISE EXCEPTION 'required predicate rechecks were not executed: %', predicate;
    END IF;
    EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) ' || statement INTO plan;
    IF plan::text NOT LIKE '%Bitmap Index Scan%' OR plan::text NOT LIKE '%exact_bitmap_pin%' THEN
        RAISE EXCEPTION 'bitmap fixture failed to select Pin: %', plan;
    END IF;
    IF memory = '64kB' AND COALESCE(jsonb_path_query_first(plan, '$.**."Lossy Heap Blocks"')::int, 0) = 0 THEN
        RAISE EXCEPTION 'low-memory fixture did not exercise bitmap lossification: %', plan;
    END IF;
END
$$;
SELECT pg_temp.check_exact_bitmap(format('body OPERATOR(pin.@@@) pin.parse_query(%L)', q), true)
FROM unnest(ARRAY['alpha', 'café', 'alpha AND beta', 'alpha OR beta', '(alpha OR beta) AND alpha']) q;
SELECT pg_temp.check_exact_bitmap(format('body OPERATOR(pin.@@@) pin.parse_query(%L)', q), false)
FROM unnest(ARRAY['"alpha beta"', '"alpha alpha"', 'NOT alpha', 'alph*']) q;
-- a selected key cannot prove all keys, including a contradictory key.
SELECT pg_temp.check_exact_bitmap(
    'body OPERATOR(pin.@@@) pin.parse_query(''alpha'') AND body OPERATOR(pin.@@@) pin.parse_query(''beta'')', false);
SELECT pg_temp.check_exact_bitmap(
    'body OPERATOR(pin.@@@) pin.parse_query(''alpha'') AND body OPERATOR(pin.@@@) pin.parse_query(''NOT alpha'')', false);
SELECT pg_temp.check_exact_bitmap('body OPERATOR(pin.@@@) pin.parse_query(''alpha'')', false, '64kB');
SET pin.enable_exact_bitmap = off;
SELECT pg_temp.check_exact_bitmap('body OPERATOR(pin.@@@) pin.parse_query(''alpha'')', false);
SET pin.enable_exact_bitmap = on;
-- HOT, indexed updates and own writes retain core visibility semantics.
UPDATE public.exact_bitmap_docs SET pad = 'changed' WHERE id % 8 = 0;
UPDATE public.exact_bitmap_docs SET body = 'beta' WHERE id % 16 = 2;
DELETE FROM public.exact_bitmap_docs WHERE id % 16 = 3;
SELECT pg_temp.check_exact_bitmap('body OPERATOR(pin.@@@) pin.parse_query(''alpha'')', true);
BEGIN;
SAVEPOINT changed;
INSERT INTO public.exact_bitmap_docs VALUES (20001, 'alpha', 'own write');
SELECT pg_temp.check_exact_bitmap('body OPERATOR(pin.@@@) pin.parse_query(''alpha'')', true);
ROLLBACK TO changed;
SELECT pg_temp.check_exact_bitmap('body OPERATOR(pin.@@@) pin.parse_query(''alpha'')', true);
COMMIT;
VACUUM (ANALYZE, INDEX_CLEANUP ON) public.exact_bitmap_docs;
INSERT INTO public.exact_bitmap_docs
SELECT 30000 + i, 'beta', repeat('y', 1000) FROM generate_series(1, 4000) i;
SELECT pg_temp.check_exact_bitmap('body OPERATOR(pin.@@@) pin.parse_query(''alpha OR beta'')', true);
DROP TABLE public.exact_bitmap_docs;
RESET track_functions;
