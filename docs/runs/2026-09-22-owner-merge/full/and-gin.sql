SELECT count(*) FROM ONLY public.pin_g6_bench WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'alpha & beta');
