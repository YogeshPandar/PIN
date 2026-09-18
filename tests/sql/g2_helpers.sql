\set ON_ERROR_STOP on

CREATE OR REPLACE FUNCTION pg_temp.g2_result(target regclass, source text, use_index boolean)
RETURNS bigint[]
LANGUAGE plpgsql
AS $$
DECLARE
    statement text;
    rows bigint[];
    plan json;
BEGIN
    PERFORM set_config('enable_indexscan', 'off', true);
    PERFORM set_config('enable_indexonlyscan', 'off', true);
    PERFORM set_config('enable_seqscan', CASE WHEN use_index THEN 'off' ELSE 'on' END, true);
    PERFORM set_config('enable_bitmapscan', CASE WHEN use_index THEN 'on' ELSE 'off' END, true);

    statement := format(
        'SELECT COALESCE(array_agg(id ORDER BY id), ARRAY[]::bigint[]) FROM %s WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)',
        target,
        source
    );

    IF use_index THEN
        EXECUTE 'EXPLAIN (FORMAT JSON) ' || statement INTO plan;
        IF plan::text NOT LIKE '%Bitmap Index Scan%' THEN
            RAISE EXCEPTION 'Pin G2 expected a bitmap index plan for % on %: %',
                source, target, plan;
        END IF;
    END IF;

    EXECUTE statement INTO rows;
    RETURN rows;
END;
$$;

CREATE OR REPLACE FUNCTION pg_temp.g2_expect(target regclass, source text, expected bigint[])
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    sequential bigint[];
    indexed bigint[];
BEGIN
    sequential := pg_temp.g2_result(target, source, false);
    indexed := pg_temp.g2_result(target, source, true);
    IF sequential IS DISTINCT FROM expected OR indexed IS DISTINCT FROM expected THEN
        RAISE EXCEPTION 'Pin G2 mismatch for % on %: expected %, sequential %, indexed %',
            source, target, expected, sequential, indexed;
    END IF;
END;
$$;
