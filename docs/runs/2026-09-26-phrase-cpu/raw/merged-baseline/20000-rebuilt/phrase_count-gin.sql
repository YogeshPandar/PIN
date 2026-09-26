SELECT count(*) FROM ONLY pin_grouped_count_bench.documents WHERE to_tsvector('simple',body) @@ to_tsquery('simple','bravo <-> charlie');
