\set ON_ERROR_STOP on
SET search_path = pg_catalog;
CREATE ROLE pin_g0_reader;
DO $test$
DECLARE
    before_count bigint := pin.g0_drop_count();
    detail_text text;
    hook_oid oid;
BEGIN
    FOR hook_oid IN
        SELECT p.oid FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
        WHERE n.nspname = 'pin'
          AND (p.proname LIKE 'g0_%' OR p.proname LIKE 'g2_%')
    LOOP
        IF has_function_privilege('pin_g0_reader', hook_oid, 'EXECUTE') THEN
            RAISE EXCEPTION 'test hook is executable by an unprivileged role';
        END IF;
    END LOOP;
    FOR n IN 1..32 LOOP
        BEGIN
            PERFORM pin.g0_raise_error();
            RAISE EXCEPTION 'missing Pin error';
        EXCEPTION WHEN feature_not_supported THEN
            NULL;
        END;
        BEGIN
            PERFORM pin.g0_raise_panic();
            RAISE EXCEPTION 'missing Rust panic';
        EXCEPTION WHEN internal_error THEN
            IF SQLERRM NOT LIKE '%Pin G0 deliberate test panic%' THEN
                RAISE;
            END IF;
        END;
        BEGIN
            PERFORM pin.g0_raise_pg_error();
            RAISE EXCEPTION 'missing PostgreSQL error';
        EXCEPTION WHEN division_by_zero THEN
            GET STACKED DIAGNOSTICS detail_text = PG_EXCEPTION_DETAIL;
            IF SQLERRM <> 'Pin G0 PostgreSQL error'
               OR detail_text IS DISTINCT FROM 'Pin G0 retained detail' THEN
                RAISE EXCEPTION 'PostgreSQL error context was lost';
            END IF;
        END;
        BEGIN
            CREATE OPERATOR CLASS public.g0_error_ops FOR TYPE text USING pin AS
                OPERATOR 1 pg_catalog.= (text, text);
            RAISE EXCEPTION 'missing registered callback error';
        EXCEPTION WHEN feature_not_supported THEN
            NULL;
        END;
        IF EXISTS (SELECT FROM pg_opclass WHERE opcname = 'g0_error_ops') THEN
            RAISE EXCEPTION 'failed opclass creation survived subtransaction rollback';
        END IF;
        IF pin.abi_check() IS DISTINCT FROM true THEN
            RAISE EXCEPTION 'backend did not recover after subtransaction errors';
        END IF;
    END LOOP;
    IF pin.g0_drop_count() <> before_count + 128 THEN
        RAISE EXCEPTION 'guarded errors did not run all Rust destructors';
    END IF;
END;
$test$;
DROP ROLE pin_g0_reader;
SELECT 1;
