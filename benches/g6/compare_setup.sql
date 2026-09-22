\set ON_ERROR_STOP on
-- create only in an empty, disposable benchmark database.
\ir setup.sql
CREATE INDEX pin_g6_bench_body_gin ON public.pin_g6_bench
    USING gin(to_tsvector('simple', body));
UPDATE public.pin_g6_bench SET body = body || ' rareplanet' WHERE id <= 20;
VACUUM (ANALYZE, INDEX_CLEANUP ON) public.pin_g6_bench;
