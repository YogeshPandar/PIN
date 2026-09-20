\set ON_ERROR_STOP on
-- use a disposable database; fail rather than overwrite an existing fixture.
CREATE TABLE public.pin_g6_bench (
    id bigint PRIMARY KEY,
    body text NOT NULL,
    updates bigint NOT NULL DEFAULT 0
) WITH (fillfactor = 70);
INSERT INTO public.pin_g6_bench(id, body)
SELECT i, CASE i % 4
    WHEN 0 THEN 'ALPHA ' || repeat('beta gamma ', 64)
    WHEN 1 THEN repeat('beta gamma ', 64) || 'alpha'
    WHEN 2 THEN 'CAFÉ cafe' || chr(769) || ' alpha Σ ς ' || repeat('beta ', 64)
    ELSE repeat('beta gamma ', 64) END
FROM generate_series(1, 20000) AS i;
CREATE INDEX pin_g6_bench_body ON public.pin_g6_bench USING pin(body pin.text_ops);
VACUUM (ANALYZE, INDEX_CLEANUP ON) public.pin_g6_bench;
