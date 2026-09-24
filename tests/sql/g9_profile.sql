\set ON_ERROR_STOP on
-- this fixture is created only by the disposable normal-build g9 cluster runner.
SET pin.enable_grouped_storage = on;
CREATE TABLE public.pin_g6_bench(id bigint PRIMARY KEY, body text NOT NULL);
INSERT INTO public.pin_g6_bench
SELECT i, CASE i % 4
    WHEN 0 THEN 'ALPHA ' || repeat('beta gamma ', 64)
    WHEN 1 THEN repeat('beta gamma ', 64) || 'alpha'
    WHEN 2 THEN 'alpha ' || repeat('beta ', 64)
    ELSE repeat('beta gamma ', 64) END
FROM generate_series(1, 1024) AS i;
CREATE INDEX pin_g6_bench_body ON public.pin_g6_bench USING pin(body pin.text_ops);
CREATE INDEX pin_g6_bench_body_gin ON public.pin_g6_bench
    USING gin(to_tsvector('simple', body));
UPDATE public.pin_g6_bench SET body = body || ' rareplanet' WHERE id <= 20;
VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0) public.pin_g6_bench;
