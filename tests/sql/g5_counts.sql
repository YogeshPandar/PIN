\set ON_ERROR_STOP on
SET search_path = pg_catalog;
SET max_parallel_workers_per_gather = 0;

CREATE OR REPLACE FUNCTION pg_temp.g5_expect(statement text, expected bigint,
                                             custom boolean,
                                             certified boolean DEFAULT false)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    baseline bigint;
    counted bigint;
    plan jsonb;
    root jsonb;
BEGIN
    PERFORM set_config('pin.enable_count_fastpath', 'off', true);
    PERFORM set_config('enable_seqscan', 'on', true);
    PERFORM set_config('enable_bitmapscan', 'off', true);
    PERFORM set_config('enable_indexscan', 'off', true);
    PERFORM set_config('enable_indexonlyscan', 'off', true);
    EXECUTE statement INTO baseline;
    IF expected IS NOT NULL AND baseline IS DISTINCT FROM expected THEN
        RAISE EXCEPTION 'G5 sequential oracle mismatch: %, expected %, got %',
            statement, expected, baseline;
    END IF;

    PERFORM set_config('enable_seqscan', 'off', true);
    PERFORM set_config('enable_bitmapscan', 'on', true);
    PERFORM set_config('pin.enable_count_fastpath', 'on', true);
    PERFORM set_config('pin.enable_count_vm', 'off', true);
    EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) ' || statement INTO plan;
    root := plan->0->'Plan';
    IF custom IS NOT NULL AND
       (COALESCE(root->>'Custom Plan Provider', '') = 'PinCount') IS DISTINCT FROM custom THEN
        RAISE EXCEPTION 'G5 unexpected count plan: %', plan;
    END IF;
    IF custom IS TRUE AND COALESCE((root->>'VM Certified Roots')::bigint, -1) <> 0 THEN
        RAISE EXCEPTION 'G5 VM-disabled path certified a root: %', plan;
    END IF;
    EXECUTE statement INTO counted;
    IF counted IS DISTINCT FROM baseline THEN
        RAISE EXCEPTION 'G5 heap count mismatch: %, baseline %, got %',
            statement, baseline, counted;
    END IF;

    PERFORM set_config('pin.enable_count_vm', 'on', true);
    EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) ' || statement INTO plan;
    root := plan->0->'Plan';
    IF certified AND COALESCE((root->>'VM Certified Roots')::bigint, 0) <= 0 THEN
        RAISE EXCEPTION 'G5 fixture did not exercise certified sealed roots: %', plan;
    END IF;
    EXECUTE statement INTO counted;
    IF counted IS DISTINCT FROM baseline THEN
        RAISE EXCEPTION 'G5 mixed count mismatch: %, baseline %, got %',
            statement, baseline, counted;
    END IF;
END;
$$;

CREATE TABLE public.g5_docs(
    id bigint PRIMARY KEY,
    body text,
    pad integer NOT NULL DEFAULT 0
) WITH (fillfactor = 50, autovacuum_enabled = false);

INSERT INTO public.g5_docs(id, body)
SELECT id, CASE WHEN id % 5 = 0 THEN 'beta' ELSE 'alpha alpha beta' END
FROM generate_series(1, 2400) AS id;
INSERT INTO public.g5_docs(id, body) VALUES (-1, NULL), (-2, '');
CREATE INDEX g5_docs_pin ON public.g5_docs USING pin(body);
ANALYZE public.g5_docs;

SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1920, true);
VACUUM (INDEX_CLEANUP ON) public.g5_docs;
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1920, true, true);
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('absent')$q$, 0, true);

-- compound predicates may use the custom node but remain heap checked.
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('"alpha alpha"')$q$, 1920, false);
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha OR beta')$q$, 2400, false);
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alp*')$q$, 1920, false);

-- residual and non-plain aggregate shapes retain the core aggregate.
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha') AND id > 2000$q$, 320, false);
SELECT pg_temp.g5_expect($q$SELECT count(DISTINCT id) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1920, false);
SELECT pg_temp.g5_expect($q$SELECT count(*) FILTER (WHERE id > 2000) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 320, false);

UPDATE public.g5_docs SET pad = pad + 1 WHERE id % 7 = 0;
UPDATE public.g5_docs SET body = 'beta' WHERE id = 1;
DELETE FROM public.g5_docs WHERE id = 2;
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1918, true);

BEGIN;
SAVEPOINT g5_insert;
INSERT INTO public.g5_docs(id, body) VALUES (4000, 'alpha');
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1919, true);
ROLLBACK TO g5_insert;
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1918, true);
COMMIT;

VACUUM (INDEX_CLEANUP ON) public.g5_docs;
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1918, true, true);

BEGIN ISOLATION LEVEL SERIALIZABLE;
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1918, false);
COMMIT;

ALTER TABLE public.g5_docs ENABLE ROW LEVEL SECURITY;
CREATE POLICY g5_even ON public.g5_docs USING (id % 2 = 0);
SELECT pg_temp.g5_expect($q$SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')$q$, 1918, false);
DROP POLICY g5_even ON public.g5_docs;
ALTER TABLE public.g5_docs DISABLE ROW LEVEL SECURITY;

SET enable_seqscan = off;
SET enable_bitmapscan = on;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SET pin.enable_count_fastpath = on;
SET pin.enable_count_vm = on;
SET plan_cache_mode = force_generic_plan;
PREPARE g5_cached AS SELECT count(*) FROM ONLY public.g5_docs
    WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha');
EXECUTE g5_cached;
SET pin.enable_count_fastpath = off;
DO $$
DECLARE
    n bigint;
    plan jsonb;
BEGIN
    EXECUTE 'EXECUTE g5_cached' INTO n;
    IF n <> 1918 THEN
        RAISE EXCEPTION 'G5 cached-plan fallback mismatch';
    END IF;
    EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) EXECUTE g5_cached' INTO plan;
    IF plan->0->'Plan'->>'Custom Plan Provider' IS DISTINCT FROM 'PinCount' OR
       plan->0->'Plan'->>'Fallback' IS DISTINCT FROM 'disabled at execution' THEN
        RAISE EXCEPTION 'G5 cached plan did not exercise runtime fallback: %', plan;
    END IF;
END
$$;
DEALLOCATE g5_cached;
RESET plan_cache_mode;
RESET pin.enable_count_fastpath;
RESET pin.enable_count_vm;
RESET enable_seqscan;
RESET enable_bitmapscan;
RESET enable_indexscan;
RESET enable_indexonlyscan;
RESET max_parallel_workers_per_gather;
