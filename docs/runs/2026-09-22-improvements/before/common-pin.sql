SELECT count(*) FROM ONLY public.pin_g6_bench WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha');
