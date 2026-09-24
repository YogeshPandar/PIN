\set ON_ERROR_STOP on

-- explicit phrase results do not use the optimized predicate as its own oracle.
DO $$
DECLARE
    fixture record;
    actual boolean;
BEGIN
    FOR fixture IN SELECT * FROM (VALUES
        ('ALPHA BETA gamma', '"alpha beta"', true),
        ('alpha gamma beta', '"alpha beta"', false),
        ('alpha', '"alpha beta"', false),
        ('a a a b', '"a a b"', true),
        ('a b a', '"a a b"', false),
        ('A,,,B', '"a b"', true),
        ('CAFÉ K', '"café k"', true),
        ('', '"a b"', false)
    ) AS cases(body, query, expected)
    LOOP
        actual := fixture.body OPERATOR(pin.@@@) pin.parse_query(fixture.query);
        IF actual IS DISTINCT FROM fixture.expected THEN
            RAISE EXCEPTION 'issue14 phrase mismatch: %, %', fixture.body, fixture.query;
        END IF;
    END LOOP;
END;
$$;

-- an early hit must not hide an out-of-contract token later in the document.
DO $$
BEGIN
    BEGIN
        PERFORM ('a b ' || repeat('x', 1025)) OPERATOR(pin.@@@) pin.parse_query('"a b"');
        RAISE EXCEPTION 'issue14 phrase accepted an oversized document token';
    EXCEPTION WHEN program_limit_exceeded THEN
        NULL;
    END;
END;
$$;

-- a reused SQL expression must read the current query datum, not stale phrase state.
PREPARE issue14_phrase_query(text, pin.query) AS
    SELECT $1 OPERATOR(pin.@@@) $2;
EXECUTE issue14_phrase_query('a b c', pin.parse_query('"a b"'));
EXECUTE issue14_phrase_query('a b c', pin.parse_query('"c a"'));
DEALLOCATE issue14_phrase_query;
