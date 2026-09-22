\set ON_ERROR_STOP on
SET search_path = pg_catalog;
SET pin.enable_direct_tid_segments = on;
SET pin.enable_exact_bitmap = on;
SET pin.enable_count_fastpath = off;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE public.direct_tid_docs(id integer PRIMARY KEY, body text, pad text)
    WITH (fillfactor = 70, autovacuum_enabled = false);
INSERT INTO public.direct_tid_docs
SELECT i, CASE i % 4 WHEN 0 THEN 'alpha beta' WHEN 1 THEN 'alpha gamma'
    WHEN 2 THEN 'beta' ELSE 'gamma' END, repeat('x', 64)
FROM generate_series(1, 4000) AS i;
CREATE INDEX direct_tid_pin ON public.direct_tid_docs USING pin(body);
VACUUM (INDEX_CLEANUP ON) public.direct_tid_docs;
CREATE FUNCTION pg_temp.check_direct_tid(source text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    statement text := format('SELECT array_agg(id ORDER BY id) FROM public.direct_tid_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', source);
    expected integer[];
    actual integer[];
    plan jsonb;
BEGIN
    PERFORM set_config('enable_seqscan', 'on', true);
    PERFORM set_config('enable_bitmapscan', 'off', true);
    PERFORM set_config('enable_indexscan', 'off', true);
    EXECUTE statement INTO expected;
    PERFORM set_config('enable_seqscan', 'off', true);
    PERFORM set_config('enable_bitmapscan', 'on', true);
    EXECUTE statement INTO actual;
    IF actual IS DISTINCT FROM expected THEN
        RAISE EXCEPTION 'direct-TID result mismatch: %', source;
    END IF;
    EXECUTE 'EXPLAIN (FORMAT JSON) ' || statement INTO plan;
    IF plan::text NOT LIKE '%Bitmap Index Scan%' OR plan::text NOT LIKE '%direct_tid_pin%' THEN
        RAISE EXCEPTION 'direct-TID test did not select the Pin bitmap: %', plan;
    END IF;
END
$$;
SELECT pg_temp.check_direct_tid(q)
FROM unnest(ARRAY['alpha', 'beta', 'gamma', 'alpha AND beta', 'alpha OR gamma', '(alpha OR beta) AND gamma']) AS q;
-- Mutable writes coexist with direct sealed pages.
INSERT INTO public.direct_tid_docs VALUES (5001, 'alpha gamma', 'new');
UPDATE public.direct_tid_docs SET body = 'alpha gamma' WHERE id % 40 = 2;
SELECT pg_temp.check_direct_tid(q)
FROM unnest(ARRAY['alpha', 'beta', 'gamma', 'alpha AND beta', 'alpha OR gamma']) AS q;
-- Bulk deletion must clear every old coordinate before PostgreSQL can reuse it.
DELETE FROM public.direct_tid_docs WHERE id % 4 = 0;
VACUUM (INDEX_CLEANUP ON) public.direct_tid_docs;
INSERT INTO public.direct_tid_docs
SELECT 10000 + i, 'replacement', 'new' FROM generate_series(1, 1000) AS i;
SELECT pg_temp.check_direct_tid(q)
FROM unnest(ARRAY['alpha', 'beta', 'gamma', 'replacement', 'alpha OR replacement', 'alpha AND beta']) AS q;
VACUUM (INDEX_CLEANUP ON) public.direct_tid_docs;
SELECT pg_temp.check_direct_tid(q)
FROM unnest(ARRAY['alpha', 'replacement', 'alpha OR replacement']) AS q;
DROP TABLE public.direct_tid_docs;
RESET pin.enable_direct_tid_segments;
