\set ON_ERROR_STOP on

-- long history, upgrade, and changed suffixes use an independent heap oracle.
SET pin.enable_count_fastpath = off;
SET pin.enable_count_vm = off;
SET pin.enable_grouped_storage = on;
SET pin.enable_grouped_scan = on;
SET pin.enable_frontier_anchors = off;
SET maintenance_work_mem = '4MB';
SET work_mem = '64MB';
SET max_parallel_workers_per_gather = 0;
CREATE TABLE g9_anchor(id bigint PRIMARY KEY, body text, marker integer)
    WITH (autovacuum_enabled = false, fillfactor = 50);
INSERT INTO g9_anchor
    SELECT i, CASE WHEN i <= 20 THEN 'alpha beta rareplanet'
                   ELSE 'alpha beta' END, 0
    FROM generate_series(1, 16384) AS i;
CREATE INDEX g9_anchor_pin ON g9_anchor USING pin(body);
CREATE INDEX g9_anchor_gin ON g9_anchor USING gin(to_tsvector('simple', body));

CREATE FUNCTION pg_temp.anchor_check() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    fixture record;
    expected bigint[];
    actual bigint[];
    statement text;
    plan jsonb;
    gate text;
BEGIN
    FOR fixture IN SELECT * FROM (VALUES
        ('rareplanet', 'rareplanet', true),
        ('alpha AND rareplanet', 'alpha & rareplanet', true),
        ('rareplanet AND alpha', 'rareplanet & alpha', true),
        ('alpha AND beta', 'alpha & beta', true),
        ('alpha OR rareplanet', 'alpha | rareplanet', true),
        ('alpha AND NOT rareplanet', 'alpha & !rareplanet', true),
        ('NOT (alpha OR rareplanet)', '!(alpha | rareplanet)', true),
        ('alpha AND newterm', 'alpha & newterm', true),
        ('alpha*', 'alpha:*', false),
        ('"alpha beta"', 'alpha <-> beta', false)
    ) AS cases(pin_query, gin_query, exact)
    LOOP
        statement := format('SELECT id FROM g9_anchor WHERE '
                            'body OPERATOR(pin.@@@) pin.parse_query(%L)', fixture.pin_query);
        SET LOCAL enable_seqscan = on;
        SET LOCAL enable_bitmapscan = off;
        SET LOCAL enable_indexscan = off;
        SET LOCAL enable_indexonlyscan = off;
        EXECUTE 'SELECT array_agg(id ORDER BY id) FROM (' || statement || ') AS rows' INTO expected;
        SET LOCAL enable_seqscan = off;
        SET LOCAL enable_bitmapscan = on;
        FOREACH gate IN ARRAY ARRAY['off', 'on'] LOOP
            PERFORM set_config('pin.enable_frontier_anchors', gate, true);
            EXECUTE 'SELECT array_agg(id ORDER BY id) FROM (' || statement || ') AS rows' INTO actual;
            IF actual IS DISTINCT FROM expected THEN
                RAISE EXCEPTION 'anchor identity mismatch: %, %', gate, fixture.pin_query;
            END IF;
            EXECUTE 'EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) ' || statement INTO plan;
            IF plan #>> '{0,Plan,Node Type}' <> 'Bitmap Heap Scan'
               OR plan #>> '{0,Plan,Plans,0,Index Name}' <> 'g9_anchor_pin'
               OR COALESCE((plan #>> '{0,Plan,Lossy Heap Blocks}')::integer, 0) <> 0
               OR (fixture.exact AND COALESCE(
                    (plan #>> '{0,Plan,Rows Removed by Index Recheck}')::integer, 0) <> 0)
            THEN
                RAISE EXCEPTION 'unexpected anchor plan: %', plan;
            END IF;
        END LOOP;
        EXECUTE format('SELECT array_agg(id ORDER BY id) FROM g9_anchor '
                       'WHERE to_tsvector(''simple'', body) @@ to_tsquery(''simple'', %L)',
                       fixture.gin_query) INTO actual;
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'gin identity mismatch: %', fixture.pin_query;
        END IF;
    END LOOP;
END;
$$;

-- version-one storage stays readable, then maintenance publishes anchors.
SELECT pg_temp.anchor_check();
SET pin.enable_frontier_anchors = on;
VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0) g9_anchor;
SET pin.enable_grouped_storage = off;
SELECT pg_temp.anchor_check();
INSERT INTO g9_anchor SELECT i, 'unrelated filler', 0 FROM generate_series(16385, 17384) AS i;
SELECT pg_temp.anchor_check();
INSERT INTO g9_anchor SELECT i, 'alpha beta rareplanet', 0 FROM generate_series(17385, 17448) AS i;
SELECT pg_temp.anchor_check();
INSERT INTO g9_anchor
    SELECT i, CASE WHEN i % 3 = 0 THEN 'alpha rareplanet newterm'
                   ELSE 'beta newterm' END, 0
    FROM generate_series(17449, 21544) AS i;
SELECT pg_temp.anchor_check();

BEGIN;
INSERT INTO g9_anchor VALUES (30000, 'alpha beta rareplanet aborted', 0);
SELECT pg_temp.anchor_check();
ROLLBACK;
UPDATE g9_anchor SET marker = marker + 1 WHERE id <= 10;
UPDATE g9_anchor SET body = 'beta newterm' WHERE id = 11;
DELETE FROM g9_anchor WHERE id = 12;
SELECT pg_temp.anchor_check();

-- canonical rewrites must invalidate even when all experiment gates are off.
SET pin.enable_frontier_anchors = off;
SET pin.enable_grouped_scan = off;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_anchor;
SET pin.enable_grouped_scan = on;
SET pin.enable_frontier_anchors = on;
INSERT INTO g9_anchor VALUES (30001, 'alpha rareplanet newterm', 0);
SELECT pg_temp.anchor_check();
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_anchor;
SELECT pg_temp.anchor_check();

-- retain an anchored changed suffix for the test-hook selection and lock probes.
SET pin.enable_grouped_storage = off;
INSERT INTO g9_anchor SELECT i, 'unrelated filler', 0 FROM generate_series(31000, 31999) AS i;
INSERT INTO g9_anchor SELECT i, 'alpha beta rareplanet', 0 FROM generate_series(32000, 32063) AS i;
SELECT pg_temp.anchor_check();
RESET work_mem;
