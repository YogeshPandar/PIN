\set ON_ERROR_STOP on

SET pin.enable_count_fastpath = off;
SET pin.enable_count_vm = off;
SET pin.enable_exact_bitmap = on;
SET pin.enable_grouped_scan = on;
SET max_parallel_workers_per_gather = 0;
SET max_parallel_maintenance_workers = 0;
SET work_mem = '64MB';
SET maintenance_work_mem = '16MB';

CREATE TABLE anchor_docs(id bigint PRIMARY KEY, body text, marker integer)
    WITH (autovacuum_enabled = false, fillfactor = 50);
INSERT INTO anchor_docs
    SELECT i, CASE WHEN i <= 20 THEN 'alpha beta rareplanet'
                   ELSE 'alpha beta' END, 0 FROM generate_series(1, 16384) AS i;

-- first publish v1, then require a no-write upgrade to anchored v2.
SET pin.enable_frontier_anchors = off;
SET pin.enable_grouped_storage = on;
CREATE INDEX anchor_docs_pin ON anchor_docs USING pin(body);
CREATE INDEX anchor_docs_gin ON anchor_docs USING gin(to_tsvector('simple', body));

CREATE FUNCTION pg_temp.anchor_check() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    fixture record;
    gate text;
    expected jsonb;
    actual jsonb;
    statement text;
    plan jsonb;
BEGIN
    FOR fixture IN SELECT * FROM (VALUES
        ('alpha', 'alpha'), ('rareplanet', 'rareplanet'),
        ('alpha AND rareplanet', 'alpha & rareplanet'),
        ('rareplanet AND alpha', 'rareplanet & alpha'),
        ('alpha AND beta', 'alpha & beta'),
        ('alpha OR rareplanet', 'alpha | rareplanet'),
        ('alpha AND NOT beta', 'alpha & !beta'),
        ('NOT alpha', '!alpha'), ('NOT missing', '!missing'),
        ('(alpha OR beta) AND NOT rareplanet', '(alpha | beta) & !rareplanet'),
        ('alpha AND newterm', 'alpha & newterm'),
        ('"alpha beta"', 'alpha <-> beta'), ('alp*', 'alp:*')
    ) AS cases(pin_query, gin_query)
    LOOP
        statement := format('SELECT id, ctid FROM anchor_docs WHERE '
                            'to_tsvector(''simple'', body) @@ to_tsquery(''simple'', %L)',
                            fixture.gin_query);
        SET LOCAL enable_seqscan = on;
        SET LOCAL enable_bitmapscan = off;
        SET LOCAL enable_indexscan = off;
        SET LOCAL enable_indexonlyscan = off;
        EXECUTE 'SELECT jsonb_agg(jsonb_build_array(id, ctid::text) ORDER BY id, ctid) FROM ('
                || statement || ') AS rows' INTO expected;
        SET LOCAL enable_seqscan = off;
        SET LOCAL enable_bitmapscan = on;
        EXECUTE 'SELECT jsonb_agg(jsonb_build_array(id, ctid::text) ORDER BY id, ctid) FROM ('
                || statement || ') AS rows' INTO actual;
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'gin identity mismatch: %', fixture.gin_query;
        END IF;
        EXECUTE 'EXPLAIN (FORMAT JSON) ' || statement INTO plan;
        IF plan #>> '{0,Plan,Plans,0,Index Name}' IS DISTINCT FROM 'anchor_docs_gin' THEN
            RAISE EXCEPTION 'gin control did not use its bitmap index: %', plan;
        END IF;
        statement := format('SELECT id, ctid FROM anchor_docs WHERE '
                            'body OPERATOR(pin.@@@) pin.parse_query(%L)', fixture.pin_query);
        FOREACH gate IN ARRAY ARRAY['off', 'on'] LOOP
            PERFORM set_config('pin.enable_frontier_anchors', gate, true);
            EXECUTE 'SELECT jsonb_agg(jsonb_build_array(id, ctid::text) ORDER BY id, ctid) FROM ('
                    || statement || ') AS rows' INTO actual;
            IF actual IS DISTINCT FROM expected THEN
                RAISE EXCEPTION 'anchor identity mismatch: %, %', gate, fixture.pin_query;
            END IF;
            EXECUTE 'EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) ' || statement INTO plan;
            IF plan #>> '{0,Plan,Node Type}' IS DISTINCT FROM 'Bitmap Heap Scan'
               OR plan #>> '{0,Plan,Plans,0,Index Name}' IS DISTINCT FROM 'anchor_docs_pin'
               OR COALESCE((plan #>> '{0,Plan,Lossy Heap Blocks}')::integer, 0) <> 0
               OR COALESCE((plan #>> '{0,Plan,Rows Removed by Index Recheck}')::integer, 0) <> 0
            THEN
                RAISE EXCEPTION 'unexpected anchor bitmap plan: %', plan;
            END IF;
        END LOOP;
    END LOOP;
END;
$$;

SELECT pg_temp.anchor_check();
SET pin.enable_frontier_anchors = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) anchor_docs;
SET pin.enable_grouped_storage = off;
SELECT pg_temp.anchor_check();
INSERT INTO anchor_docs
    SELECT i, 'unrelated filler', 0 FROM generate_series(16385, 17384) AS i;
SELECT pg_temp.anchor_check();
INSERT INTO anchor_docs
    SELECT i, 'alpha beta rareplanet newterm', 0 FROM generate_series(17385, 17448) AS i;
SELECT pg_temp.anchor_check();

-- append a multi-page suffix without compacting the anchored history.
INSERT INTO anchor_docs
    SELECT i, CASE WHEN i % 2 = 0 THEN 'alpha beta rareplanet newterm'
                   ELSE 'beta newterm' END, 0 FROM generate_series(17449, 19496) AS i;
BEGIN;
INSERT INTO anchor_docs VALUES (20000, 'alpha beta rareplanet aborted', 0);
ROLLBACK;
BEGIN;
UPDATE anchor_docs SET marker = marker + 1 WHERE id <= 10;
DO $$ BEGIN
    IF pg_stat_get_xact_tuples_hot_updated('anchor_docs'::regclass) = 0 THEN
        RAISE EXCEPTION 'fixture did not exercise a hot update';
    END IF;
END $$;
COMMIT;
UPDATE anchor_docs SET body = 'newterm' WHERE id = 11;
DELETE FROM anchor_docs WHERE id = 12;
SELECT pg_temp.anchor_check();

-- invalidation and recovery cannot depend on the read gate staying enabled.
SET pin.enable_frontier_anchors = off;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) anchor_docs;
SET pin.enable_frontier_anchors = on;
SELECT pg_temp.anchor_check();
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) anchor_docs;
SELECT pg_temp.anchor_check();

CREATE TEMP TABLE anchor_old_tids AS SELECT ctid AS old_tid FROM anchor_docs;
DELETE FROM anchor_docs;
SET pin.enable_grouped_storage = off;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) anchor_docs;
INSERT INTO anchor_docs SELECT i, 'beta newterm', 0 FROM generate_series(1, 2048) AS i;
DO $$ BEGIN
    IF NOT EXISTS (SELECT FROM anchor_docs n JOIN anchor_old_tids o ON n.ctid = o.old_tid) THEN
        RAISE EXCEPTION 'fixture did not exercise physical tid reuse';
    END IF;
END $$;
SELECT pg_temp.anchor_check();
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) anchor_docs;
SELECT pg_temp.anchor_check();
DROP TABLE anchor_docs;
DROP TABLE anchor_old_tids;
RESET pin.enable_frontier_anchors;
RESET pin.enable_grouped_storage;
RESET work_mem;
RESET maintenance_work_mem;
