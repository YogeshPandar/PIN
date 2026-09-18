\set ON_ERROR_STOP on
SET search_path = pg_catalog;
\ir g2_helpers.sql

BEGIN ISOLATION LEVEL REPEATABLE READ;
SELECT count(*) FROM public.g2_concurrent;
SELECT pg_advisory_xact_lock(180006, 3);

DO $$
DECLARE
    sequential bigint[];
    indexed bigint[];
BEGIN
    sequential := pg_temp.g2_result('public.g2_concurrent'::regclass, 'alpha', false);
    indexed := pg_temp.g2_result('public.g2_concurrent'::regclass, 'alpha', true);
    IF sequential IS DISTINCT FROM indexed OR cardinality(indexed) <> 8 THEN
        RAISE EXCEPTION 'Pin G2 concurrent snapshot mismatch: sequential %, indexed %',
            sequential, indexed;
    END IF;
END;
$$;

COMMIT;
