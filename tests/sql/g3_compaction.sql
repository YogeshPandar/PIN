\set ON_ERROR_STOP on
\ir g2_helpers.sql

CREATE TABLE public.g3_docs(id bigint PRIMARY KEY, body text)
WITH (autovacuum_enabled = false, fillfactor = 70);
INSERT INTO public.g3_docs
SELECT i, 'common ' || CASE i % 4
    WHEN 0 THEN 'alpha beta alpha'
    WHEN 1 THEN 'alpha gamma'
    WHEN 2 THEN 'beta delta'
    ELSE 'omega' END
FROM generate_series(1, 6000) AS i;
CREATE INDEX g3_docs_pin ON public.g3_docs USING pin(body pin.text_ops);
ANALYZE public.g3_docs;
VACUUM (INDEX_CLEANUP ON) public.g3_docs;

CREATE TEMP TABLE g3_sizes AS
SELECT pg_relation_size('public.g3_docs_pin') AS bytes;
VACUUM (INDEX_CLEANUP ON) public.g3_docs;
DO $$
BEGIN
    IF pg_relation_size('public.g3_docs_pin') <> (SELECT bytes FROM g3_sizes) THEN
        RAISE EXCEPTION 'unchanged sealed chains must not allocate new pages';
    END IF;
END;
$$;

-- append after sealing, then remove both a subset and aborted index publications.
INSERT INTO public.g3_docs VALUES (6001, 'common alpha beta');
BEGIN;
INSERT INTO public.g3_docs VALUES (6002, 'rolledback alpha');
ROLLBACK;
UPDATE public.g3_docs SET body = 'common changed gamma' WHERE id % 5 = 0;
DELETE FROM public.g3_docs WHERE id % 7 = 0;
VACUUM (INDEX_CLEANUP ON) public.g3_docs;
INSERT INTO public.g3_docs
SELECT i, 'replacement common alpha' FROM generate_series(6100, 6200) AS i;
VACUUM (INDEX_CLEANUP ON) public.g3_docs;

DO $$
DECLARE
    source text;
    expected bigint[];
BEGIN
    FOREACH source IN ARRAY ARRAY[
        'common', 'alpha', 'beta', 'changed', 'replacement', 'rolledback',
        'alpha AND beta', 'alpha OR gamma', 'NOT absent', 'NOT alpha',
        'al*', '"alpha beta"', '"alpha alpha"', 'alpha AND NOT beta'
    ] LOOP
        expected := pg_temp.g2_result('public.g3_docs', source, false);
        PERFORM pg_temp.g2_expect('public.g3_docs', source, expected);
    END LOOP;
    PERFORM pg_temp.g2_expect('public.g3_docs', 'rolledback', ARRAY[]::bigint[]);
END;
$$;

-- a caught ERROR must release the reader barrier in the same live backend.
DO $$
BEGIN
    PERFORM pin.g2_inject(12, 1, false);
    BEGIN
        PERFORM pg_temp.g2_result('public.g3_docs', 'common', true);
        RAISE EXCEPTION 'reader-barrier injection did not fire';
    EXCEPTION WHEN internal_error THEN
        IF SQLERRM <> 'Pin injected storage error' THEN
            RAISE;
        END IF;
    END;
    IF EXISTS (
        SELECT FROM pg_locks WHERE pid = pg_backend_pid()
        AND locktype = 'page' AND relation = 'public.g3_docs_pin'::regclass
        AND page = 1
    ) THEN
        RAISE EXCEPTION 'caught reader ERROR leaked the structural barrier';
    END IF;
    PERFORM pg_temp.g2_expect('public.g3_docs', 'rolledback', ARRAY[]::bigint[]);
END;
$$;
VACUUM (INDEX_CLEANUP ON) public.g3_docs;
