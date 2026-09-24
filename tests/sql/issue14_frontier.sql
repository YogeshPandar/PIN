\set ON_ERROR_STOP on

-- standalone fixture keeps the g9 lifecycle driver's relations unchanged.
CREATE TABLE issue14_frontier(id bigint PRIMARY KEY, body text, marker integer)
    WITH (autovacuum_enabled = false, fillfactor = 50);
INSERT INTO issue14_frontier
    SELECT i, CASE WHEN i <= 20 THEN 'alpha beta rareplanet'
                   ELSE 'alpha beta' END, 0
    FROM generate_series(1, 1024) AS i;
SET pin.enable_grouped_storage = on;
CREATE INDEX issue14_frontier_pin ON issue14_frontier USING pin(body);
CREATE INDEX issue14_frontier_gin ON issue14_frontier
    USING gin(to_tsvector('simple', body));
SET pin.enable_grouped_storage = off;
SET pin.enable_grouped_scan = on;
SET work_mem = '64MB';

CREATE FUNCTION pg_temp.issue14_frontier_check() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    fixture record;
    expected bigint[];
    actual bigint[];
    plan jsonb;
    query text;
BEGIN
    FOR fixture IN SELECT * FROM (VALUES
        ('alpha', 'alpha'), ('rareplanet', 'rareplanet'),
        ('alpha AND rareplanet', 'alpha & rareplanet'),
        ('rareplanet AND alpha', 'rareplanet & alpha'),
        ('alpha AND beta', 'alpha & beta'),
        ('alpha OR rareplanet', 'alpha | rareplanet'),
        ('alpha AND NOT rareplanet', 'alpha & !rareplanet'),
        ('NOT (alpha OR rareplanet)', '!(alpha | rareplanet)'),
        ('alpha AND newterm', 'alpha & newterm')
    ) AS cases(pin_query, gin_query)
    LOOP
        query := format('SELECT id FROM issue14_frontier WHERE '
                        'body OPERATOR(pin.@@@) pin.parse_query(%L)', fixture.pin_query);
        SET LOCAL enable_seqscan = on;
        SET LOCAL enable_bitmapscan = off;
        SET LOCAL enable_indexscan = off;
        SET LOCAL enable_indexonlyscan = off;
        EXECUTE 'SELECT array_agg(id ORDER BY id) FROM (' || query || ') AS rows'
            INTO expected;
        SET LOCAL enable_seqscan = off;
        SET LOCAL enable_bitmapscan = on;
        EXECUTE 'SELECT array_agg(id ORDER BY id) FROM (' || query || ') AS rows'
            INTO actual;
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'frontier changed row identities for %', fixture.pin_query;
        END IF;
        EXECUTE format('SELECT array_agg(id ORDER BY id) FROM issue14_frontier '
                       'WHERE to_tsvector(''simple'', body) @@ to_tsquery(''simple'', %L)',
                       fixture.gin_query) INTO actual;
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'frontier disagrees with gin for %', fixture.pin_query;
        END IF;
        EXECUTE 'EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) ' || query INTO plan;
        IF plan #>> '{0,Plan,Node Type}' <> 'Bitmap Heap Scan'
           OR plan #>> '{0,Plan,Plans,0,Index Name}' <> 'issue14_frontier_pin'
           OR COALESCE((plan #>> '{0,Plan,Lossy Heap Blocks}')::integer, 0) <> 0
           OR COALESCE((plan #>> '{0,Plan,Rows Removed by Index Recheck}')::integer, 0) <> 0
        THEN
            RAISE EXCEPTION 'frontier did not produce exact bitmap candidates: %', plan;
        END IF;
    END LOOP;
END;
$$;

SELECT pg_temp.issue14_frontier_check();
INSERT INTO issue14_frontier
    SELECT i, 'unrelated filler', 0 FROM generate_series(1025, 2024) AS i;
SELECT pg_temp.issue14_frontier_check();

-- related deltas span more than one posting page without rebuilding the snapshot.
INSERT INTO issue14_frontier
    SELECT i, CASE WHEN i % 3 = 0 THEN 'alpha beta rareplanet newterm'
                   ELSE 'alpha newterm' END, 0
    FROM generate_series(2025, 4072) AS i;
BEGIN;
INSERT INTO issue14_frontier VALUES (5000, 'alpha rareplanet aborted', 0);
ROLLBACK;
UPDATE issue14_frontier SET marker = marker + 1 WHERE id <= 10;
UPDATE issue14_frontier SET body = 'newterm' WHERE id = 11;
DELETE FROM issue14_frontier WHERE id = 12;
SELECT pg_temp.issue14_frontier_check();

-- retire old memberships before reusing heap slots or publishing another snapshot.
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) issue14_frontier;
INSERT INTO issue14_frontier VALUES (5001, 'beta newterm', 0);
SELECT pg_temp.issue14_frontier_check();
SET pin.enable_grouped_storage = on;
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) issue14_frontier;
SELECT pg_temp.issue14_frontier_check();
DROP TABLE issue14_frontier;
RESET work_mem;
