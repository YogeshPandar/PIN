SELECT count(*) FROM ONLY pin_grouped_count_bench.documents WHERE body OPERATOR(pin.@@@) pin.parse_query('"bravo charlie"');
