\set ON_ERROR_STOP on
SET search_path = pg_catalog;
\ir g2_helpers.sql

SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'alpha', ARRAY[1, 5, 10]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'delta', ARRAY[2]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'upserted', ARRAY[3]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'savepoint', ARRAY[]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'abort', ARRAY[]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'speculative', ARRAY[]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_reuse'::regclass, 'alpha', ARRAY[]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_reuse'::regclass, 'beta', ARRAY[9002]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_crash_docs'::regclass, 'stable', ARRAY[1]::bigint[]);
SELECT pg_temp.g2_expect(
    'public.g2_crash_docs'::regclass,
    'crashone OR crashtwo OR crashthree OR crashfour OR crashfive OR crashsix',
    ARRAY[]::bigint[]
);

DO $$
DECLARE
    sequential bigint[];
    indexed bigint[];
BEGIN
    sequential := pg_temp.g2_result('public.g2_concurrent'::regclass, 'alpha', false);
    indexed := pg_temp.g2_result('public.g2_concurrent'::regclass, 'alpha', true);
    IF sequential IS DISTINCT FROM indexed OR cardinality(indexed) <> 72 THEN
        RAISE EXCEPTION 'Pin G2 post-restart concurrent mismatch: sequential %, indexed %',
            sequential, indexed;
    END IF;
END;
$$;

-- g4 boolean results survive both the ordinary restart and the crash matrix.
SELECT pg_temp.g2_expect('public.g4_docs', 'a AND b', ARRAY[4]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', '"a a"', ARRAY[6]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_scale', 'a AND b',
    ARRAY(SELECT id::bigint FROM generate_series(1, 4000) AS id WHERE id % 7 = 0 AND id % 5 <> 0));
