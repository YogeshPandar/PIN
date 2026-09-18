\set ON_ERROR_STOP on
\ir g2_helpers.sql

-- the fixture is committed before compaction; every identifier must survive.
SELECT set_config('g3_test.expected_rows', :'expected_rows', false);
DO $$
DECLARE
    expected bigint[];
    source text;
BEGIN
    SELECT array_agg(i ORDER BY i) INTO expected
    FROM generate_series(1, current_setting('g3_test.expected_rows')::bigint) AS i;
    FOREACH source IN ARRAY ARRAY[
        'alpha', 'beta', 'common', 'alpha AND beta', 'alpha OR beta',
        'al*', '"alpha beta"', 'NOT absent'
    ] LOOP
        PERFORM pg_temp.g2_expect('public.g3_fault_docs', source, expected);
    END LOOP;
    PERFORM pg_temp.g2_expect('public.g3_fault_docs', 'absent', ARRAY[]::bigint[]);
END;
$$;
