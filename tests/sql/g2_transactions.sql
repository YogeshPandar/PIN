\set ON_ERROR_STOP on
SET search_path = pg_catalog;
\ir g2_helpers.sql

CREATE TABLE public.g2_docs (
    id bigint PRIMARY KEY,
    body text NOT NULL,
    pad integer NOT NULL DEFAULT 0
) WITH (fillfactor = 50);

INSERT INTO public.g2_docs(id, body) VALUES
    (1, 'alpha base'),
    (2, 'beta hot'),
    (3, 'gamma phrase gamma'),
    (4, ''),
    (5, 'alpha beta');

CREATE INDEX g2_docs_pin ON public.g2_docs USING pin(body);
ANALYZE public.g2_docs;

SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'alpha', ARRAY[1, 5]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, '"gamma phrase"', ARRAY[3]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'NOT alpha', ARRAY[2, 3, 4]::bigint[]);

BEGIN;
INSERT INTO public.g2_docs(id, body) VALUES (10, 'ownwrite alpha');
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'ownwrite', ARRAY[10]::bigint[]);

SAVEPOINT g2_savepoint;
INSERT INTO public.g2_docs(id, body) VALUES (11, 'savepoint ghost');
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'savepoint', ARRAY[11]::bigint[]);
ROLLBACK TO g2_savepoint;
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'savepoint', ARRAY[]::bigint[]);
COMMIT;

BEGIN;
INSERT INTO public.g2_docs(id, body) VALUES (12, 'abort ghost');
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'abort', ARRAY[12]::bigint[]);
ROLLBACK;
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'abort', ARRAY[]::bigint[]);

INSERT INTO public.g2_docs(id, body)
VALUES (1, 'speculative ghost')
ON CONFLICT (id) DO NOTHING;
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'speculative', ARRAY[]::bigint[]);

SELECT pg_stat_reset_single_table_counters('public.g2_docs'::regclass);
UPDATE public.g2_docs SET pad = pad + 1 WHERE id = 2;
SELECT pg_stat_force_next_flush();

DO $$
DECLARE
    hot_updates bigint;
BEGIN
    SELECT n_tup_hot_upd
    INTO hot_updates
    FROM pg_stat_all_tables
    WHERE relid = 'public.g2_docs'::regclass;

    IF hot_updates < 1 THEN
        RAISE EXCEPTION 'Pin G2 HOT fixture did not produce a HOT update';
    END IF;
END;
$$;

SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'hot', ARRAY[2]::bigint[]);

UPDATE public.g2_docs SET body = 'beta changed delta' WHERE id = 2;
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'hot', ARRAY[]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'delta', ARRAY[2]::bigint[]);

INSERT INTO public.g2_docs(id, body)
VALUES (3, 'gamma upserted')
ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body;
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'upserted', ARRAY[3]::bigint[]);

VACUUM (INDEX_CLEANUP ON) public.g2_docs;
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'alpha', ARRAY[1, 5, 10]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_docs'::regclass, 'delta', ARRAY[2]::bigint[]);

CREATE TABLE public.g2_reuse (
    id bigint PRIMARY KEY,
    body text NOT NULL
);

INSERT INTO public.g2_reuse VALUES (9001, 'reuse alpha');
CREATE INDEX g2_reuse_pin ON public.g2_reuse USING pin(body);
CREATE TEMP TABLE g2_old_tid(tid tid);
INSERT INTO g2_old_tid SELECT ctid FROM public.g2_reuse WHERE id = 9001;

DELETE FROM public.g2_reuse WHERE id = 9001;
VACUUM (INDEX_CLEANUP ON) public.g2_reuse;

INSERT INTO public.g2_reuse VALUES (9002, 'reuse beta');

DO $$
DECLARE
    old_tid tid;
    new_tid tid;
BEGIN
    SELECT tid INTO STRICT old_tid FROM pg_temp.g2_old_tid;
    SELECT ctid INTO STRICT new_tid FROM public.g2_reuse WHERE id = 9002;
    IF new_tid <> old_tid THEN
        RAISE EXCEPTION 'Pin G2 slot-reuse fixture did not reuse the physical TID: old %, new %',
            old_tid, new_tid;
    END IF;
END;
$$;

SELECT pg_temp.g2_expect('public.g2_reuse'::regclass, 'alpha', ARRAY[]::bigint[]);
SELECT pg_temp.g2_expect('public.g2_reuse'::regclass, 'beta', ARRAY[9002]::bigint[]);

CREATE TABLE public.g2_concurrent (
    id bigint PRIMARY KEY,
    body text NOT NULL
);

INSERT INTO public.g2_concurrent
SELECT 20000 + id, 'concurrent alpha seed'
FROM generate_series(1, 8) AS id;

CREATE INDEX g2_concurrent_pin ON public.g2_concurrent USING pin(body);
ANALYZE public.g2_concurrent;

CREATE TABLE public.g2_crash_docs (
    id bigint PRIMARY KEY,
    body text NOT NULL
);

INSERT INTO public.g2_crash_docs VALUES (1, 'stable baseline');
CREATE INDEX g2_crash_docs_pin ON public.g2_crash_docs USING pin(body);

SELECT pg_temp.g2_expect('public.g2_crash_docs'::regclass, 'stable', ARRAY[1]::bigint[]);
