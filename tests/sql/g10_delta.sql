\set ON_ERROR_STOP on
\ir g2_helpers.sql

SET max_parallel_maintenance_workers = 0;
SET max_parallel_workers_per_gather = 0;
SET pin.enable_count_fastpath = on;
SET pin.enable_grouped_count = on;
SET pin.enable_count_vm = on;
SET pin.enable_grouped_page_visibility = on;
SET pin.enable_grouped_storage = on;
SET pin.enable_grouped_scan = on;
SET pin.enable_grouped_delta_seal = on;

CREATE TABLE g10_docs(id bigint PRIMARY KEY, body text NOT NULL, note integer DEFAULT 0)
    WITH (autovacuum_enabled = false, fillfactor = 60);
INSERT INTO g10_docs(id, body)
SELECT n, CASE WHEN n % 3 = 0 THEN 'alpha beta' ELSE 'alpha gamma' END
FROM generate_series(1, 1200) n;
CREATE INDEX g10_docs_pin ON g10_docs USING pin(body);
CREATE INDEX g10_docs_gin ON g10_docs USING gin(to_tsvector('simple', body));

CREATE FUNCTION pg_temp.g10_check() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    query text;
    expected bigint[];
    actual bigint[];
    counted bigint;
BEGIN
    FOREACH query IN ARRAY ARRAY[
        'alpha', 'alpha AND beta', 'alpha OR beta', 'alpha AND NOT beta',
        'NOT beta', 'newterm', 'alpha AND newterm', '"alpha beta"'
    ] LOOP
        expected := pg_temp.g2_result('g10_docs', query, false);
        actual := pg_temp.g2_result('g10_docs', query, true);
        IF actual IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'g10 mismatch for %: expected %, actual %', query, expected, actual;
        END IF;
        EXECUTE format('SELECT count(*) FROM g10_docs WHERE body OPERATOR(pin.@@@) pin.parse_query(%L)', query)
            INTO counted;
        IF counted <> cardinality(expected) THEN
            RAISE EXCEPTION 'g10 count mismatch for %: expected %, actual %', query, cardinality(expected), counted;
        END IF;
    END LOOP;
END;
$$;

SELECT pg_temp.g10_check();
INSERT INTO g10_docs(id, body)
SELECT n, CASE WHEN n % 2 = 0 THEN 'alpha beta newterm' ELSE 'gamma newterm' END
FROM generate_series(1201, 1713) n;
SELECT pg_temp.g10_check();
INSERT INTO g10_docs(id, body)
SELECT n, CASE WHEN n % 2 = 0 THEN 'alpha beta newterm' ELSE 'gamma newterm' END
FROM generate_series(1714, 2226) n;
SELECT pg_temp.g10_check();
INSERT INTO g10_docs(id, body)
SELECT n, CASE WHEN n % 2 = 0 THEN 'alpha beta newterm' ELSE 'gamma newterm' END
FROM generate_series(2227, 2739) n;
SELECT pg_temp.g10_check();
INSERT INTO g10_docs(id, body)
SELECT n, CASE WHEN n % 2 = 0 THEN 'alpha beta newterm' ELSE 'gamma newterm' END
FROM generate_series(2740, 3252) n;
SELECT pg_temp.g10_check();
UPDATE g10_docs SET note = note + 1 WHERE id % 101 = 0;
UPDATE g10_docs SET body = 'beta changed' WHERE id % 103 = 0;
DELETE FROM g10_docs WHERE id % 107 = 0;
SELECT pg_temp.g10_check();
VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g10_docs;
SELECT pg_temp.g10_check();
INSERT INTO g10_docs VALUES (4000, 'alpha beta newterm');
SELECT pg_temp.g10_check();
REINDEX INDEX g10_docs_pin;
SELECT pg_temp.g10_check();
