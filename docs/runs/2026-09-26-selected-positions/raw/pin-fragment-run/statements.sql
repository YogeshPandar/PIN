SELECT pg_backend_pid();
SELECT pin.build_revision();
SELECT version();
CREATE TABLE public.pin_fragment_7326eeda6a6f(id int PRIMARY KEY, body text) WITH (autovacuum_enabled=false);
INSERT INTO public.pin_fragment_7326eeda6a6f SELECT i, repeat('echo ',10000) || CASE WHEN i%2=0 THEN 'alpha beta' ELSE 'beta alpha' END FROM generate_series(1,256) i;
CREATE INDEX ON public.pin_fragment_7326eeda6a6f USING pin(body);
CREATE INDEX ON public.pin_fragment_7326eeda6a6f USING gin(to_tsvector('simple',body));
VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0) public.pin_fragment_7326eeda6a6f;
SET pin.enable_count_fastpath=off; SET pin.enable_grouped_count=off;
BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET pin.enable_phrase_positions=off;SET enable_seqscan=on;SET enable_bitmapscan=off;
SELECT id,ctid FROM ONLY public.pin_fragment_7326eeda6a6f WHERE body OPERATOR(pin.@@@) pin.parse_query('"alpha beta"') ORDER BY id,ctid;
EXPLAIN (ANALYZE,BUFFERS,TIMING OFF,FORMAT JSON) SELECT count(*) FROM ONLY public.pin_fragment_7326eeda6a6f WHERE body OPERATOR(pin.@@@) pin.parse_query('"alpha beta"')
ROLLBACK;
DROP TABLE public.pin_fragment_7326eeda6a6f;
