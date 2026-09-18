\set ON_ERROR_STOP on
SET search_path = pg_catalog;
\ir g2_helpers.sql

CREATE TABLE public.g4_docs (
    id bigint PRIMARY KEY,
    body text,
    pad integer NOT NULL DEFAULT 0
) WITH (fillfactor = 50, autovacuum_enabled = false);

INSERT INTO public.g4_docs(id, body) VALUES
    (0, ''), (1, 'a'), (2, 'b'), (3, 'c'), (4, 'a b'),
    (5, 'b a'), (6, 'a a'), (7, 'a b c'), (8, 'alphabet'), (9, NULL);
CREATE INDEX g4_docs_pin ON public.g4_docs USING pin(body);
ANALYZE public.g4_docs;

SELECT pg_temp.g2_expect('public.g4_docs', 'a AND b', ARRAY[4, 5, 7]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', 'a OR b', ARRAY[1, 2, 4, 5, 6, 7]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', 'a OR (a AND c)', ARRAY[1, 4, 5, 6, 7]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', '(a OR b) AND (b OR c)', ARRAY[2, 4, 5, 7]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', '"a b"', ARRAY[4, 7]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', '"a a"', ARRAY[6]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', 'NOT a', ARRAY[0, 2, 3, 8]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', 'a AND NOT b', ARRAY[1, 6]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', 'a* AND NOT b', ARRAY[1, 6, 8]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', 'a AND missing', ARRAY[]::bigint[]);

BEGIN;
INSERT INTO public.g4_docs(id, body) VALUES (10, 'a b');
SELECT pg_temp.g2_expect('public.g4_docs', 'a AND b', ARRAY[4, 5, 7, 10]::bigint[]);
SAVEPOINT before_change;
UPDATE public.g4_docs SET body = 'a c' WHERE id = 10;
SELECT pg_temp.g2_expect('public.g4_docs', 'a AND b', ARRAY[4, 5, 7]::bigint[]);
ROLLBACK TO before_change;
SELECT pg_temp.g2_expect('public.g4_docs', 'a AND b', ARRAY[4, 5, 7, 10]::bigint[]);
ROLLBACK;
SELECT pg_temp.g2_expect('public.g4_docs', 'a AND b', ARRAY[4, 5, 7]::bigint[]);

-- only a qualifying live owner may survive the Boolean intersection.
UPDATE public.g4_docs SET pad = pad + 1 WHERE id = 4;
UPDATE public.g4_docs SET body = 'a c' WHERE id = 5;
DELETE FROM public.g4_docs WHERE id = 7;
VACUUM (INDEX_CLEANUP ON) public.g4_docs;
SELECT pg_temp.g2_expect('public.g4_docs', 'a AND b', ARRAY[4]::bigint[]);
SELECT pg_temp.g2_expect('public.g4_docs', '"a b"', ARRAY[4]::bigint[]);

CREATE TABLE public.g4_scale(id bigint PRIMARY KEY, body text NOT NULL)
    WITH (autovacuum_enabled = false);
INSERT INTO public.g4_scale
SELECT id, CASE WHEN id % 7 = 0 THEN 'a b' ELSE 'a' END
FROM generate_series(1, 3300) AS id;
CREATE INDEX g4_scale_pin ON public.g4_scale USING pin(body);
VACUUM (INDEX_CLEANUP ON) public.g4_scale;
INSERT INTO public.g4_scale
SELECT id, CASE WHEN id % 7 = 0 THEN 'a b' ELSE 'a' END
FROM generate_series(3301, 4000) AS id;
ANALYZE public.g4_scale;

SELECT pg_temp.g2_expect('public.g4_scale', 'a AND b',
    ARRAY(SELECT id::bigint FROM generate_series(1, 4000) AS id WHERE id % 7 = 0));
SELECT pg_temp.g2_expect('public.g4_scale', 'a OR b',
    ARRAY(SELECT id::bigint FROM generate_series(1, 4000) AS id));

-- multiple scan keys remain implicitly ANDed by the heap recheck.
DO $$
DECLARE
    rows bigint[];
    plan json;
    statement text := $q$SELECT array_agg(id ORDER BY id) FROM public.g4_scale
        WHERE body OPERATOR(pin.@@@) pin.parse_query('a OR b')
          AND body OPERATOR(pin.@@@) pin.parse_query('a AND b')$q$;
BEGIN
    PERFORM set_config('enable_seqscan', 'off', true);
    PERFORM set_config('enable_bitmapscan', 'on', true);
    PERFORM set_config('enable_indexscan', 'off', true);
    EXECUTE 'EXPLAIN (FORMAT JSON) ' || statement INTO plan;
    IF plan::text NOT LIKE '%Bitmap Index Scan%' THEN
        RAISE EXCEPTION 'Pin G4 missing multi-key bitmap plan: %', plan;
    END IF;
    EXECUTE statement INTO rows;
    IF rows IS DISTINCT FROM ARRAY(SELECT id::bigint FROM generate_series(1, 4000) AS id WHERE id % 7 = 0) THEN
        RAISE EXCEPTION 'Pin G4 multi-key recheck mismatch';
    END IF;
END;
$$;

DELETE FROM public.g4_scale WHERE id % 5 = 0;
VACUUM (INDEX_CLEANUP ON) public.g4_scale;
SELECT pg_temp.g2_expect('public.g4_scale', 'a AND b',
    ARRAY(SELECT id::bigint FROM generate_series(1, 4000) AS id WHERE id % 7 = 0 AND id % 5 <> 0));
