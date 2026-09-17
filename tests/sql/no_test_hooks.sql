\set ON_ERROR_STOP on
DO $test$
BEGIN
    IF EXISTS (
        SELECT FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
        WHERE n.nspname = 'pin' AND p.proname LIKE 'g0_%'
    ) THEN
        RAISE EXCEPTION 'test hooks present in an ordinary build';
    END IF;
END;
$test$;
