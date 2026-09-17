\set ON_ERROR_STOP on
DO $test$
BEGIN
    -- repeat in one backend to detect accidental initialization after failure.
    FOR attempt IN 1..2 LOOP
        BEGIN
            LOAD 'pin';
            RAISE EXCEPTION 'late LOAD unexpectedly succeeded';
        EXCEPTION WHEN object_not_in_prerequisite_state THEN
            IF SQLERRM NOT LIKE '%shared_preload_libraries%' THEN
                RAISE;
            END IF;
        END;
    END LOOP;
    BEGIN
        CREATE EXTENSION pin;
        RAISE EXCEPTION 'CREATE EXTENSION without preload unexpectedly succeeded';
    EXCEPTION WHEN object_not_in_prerequisite_state THEN
        IF SQLERRM NOT LIKE '%shared_preload_libraries%' THEN
            RAISE;
        END IF;
    END;
    IF EXISTS (SELECT FROM pg_extension WHERE extname = 'pin') THEN
        RAISE EXCEPTION 'failed installation left an extension behind';
    END IF;
END;
$test$;
SELECT 1;
