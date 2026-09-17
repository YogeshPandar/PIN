\set ON_ERROR_STOP on
DO $test$
BEGIN
    BEGIN
        CREATE EXTENSION pin;
        RAISE EXCEPTION 'Pin installed into a non-UTF8 database';
    EXCEPTION WHEN feature_not_supported THEN
        IF SQLERRM NOT LIKE '%UTF-8%' THEN
            RAISE;
        END IF;
    END;
    IF EXISTS (SELECT FROM pg_extension WHERE extname='pin') THEN
        RAISE EXCEPTION 'failed installation left an extension behind';
    END IF;
END;
$test$;
