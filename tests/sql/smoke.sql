\set ON_ERROR_STOP on
CREATE EXTENSION pin;
SET search_path = pg_catalog;

DO $test$
DECLARE
    am oid;
BEGIN
    SELECT oid INTO STRICT am FROM pg_am WHERE amname = 'pin' AND amtype = 'i';
    IF pin.abi_check() IS DISTINCT FROM true THEN
        RAISE EXCEPTION 'ABI check failed';
    END IF;
    IF NOT coalesce(pin.max_heap_offsets() BETWEEN 1 AND 512, false) THEN
        RAISE EXCEPTION 'invalid host heap capacity';
    END IF;
    IF NOT coalesce(pin.generic_wal_page_limit() > 0, false) THEN
        RAISE EXCEPTION 'invalid generic WAL limit';
    END IF;
    IF pg_indexam_has_property(am, 'can_order') IS DISTINCT FROM false
       OR pg_indexam_has_property(am, 'can_unique') IS DISTINCT FROM false
       OR pg_indexam_has_property(am, 'can_multi_col') IS DISTINCT FROM false
       OR pg_indexam_has_property(am, 'can_include') IS DISTINCT FROM false THEN
        RAISE EXCEPTION 'unsupported AM capability was advertised';
    END IF;
    IF NOT EXISTS (
        SELECT FROM pg_proc WHERE oid = (SELECT amhandler FROM pg_am WHERE oid = am)
        AND prorettype = 'index_am_handler'::regtype
        AND pronargs = 1 AND proargtypes[0] = 'internal'::regtype AND NOT proisstrict
    ) THEN
        RAISE EXCEPTION 'handler has the wrong SQL signature or strictness';
    END IF;
    IF (SELECT count(*) FROM pg_opclass WHERE opcmethod = am) <> 1 THEN
        RAISE EXCEPTION 'G2 must install exactly one reviewed opclass';
    END IF;
    IF EXISTS (SELECT FROM pg_proc p JOIN pg_namespace n ON p.pronamespace=n.oid
        WHERE n.nspname='pin' AND (p.proleakproof OR p.prosecdef)) THEN
        RAISE EXCEPTION 'unexpected leakproof/security-definer function';
    END IF;
END;
$test$;

CREATE TABLE public.g0_documents (body text);
INSERT INTO public.g0_documents VALUES ('alpha'), ('beta'), ('');
DO $test$
DECLARE
    original_count bigint;
BEGIN
    SELECT count(*) INTO original_count FROM public.g0_documents;
    CREATE INDEX g0_documents_pin ON public.g0_documents USING pin(body);
    IF (SELECT count(*) FROM public.g0_documents
        WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha')) <> 1 THEN
        RAISE EXCEPTION 'G2 basic match failed';
    END IF;
    BEGIN
        CREATE OPERATOR CLASS public.g0_untrusted_ops FOR TYPE text USING pin AS
            OPERATOR 1 pg_catalog.= (text, text);
        RAISE EXCEPTION 'G0 accepted an unreviewed opclass';
    EXCEPTION WHEN feature_not_supported THEN
        NULL; -- amadjustmembers rejects unreviewed members.
    END;
    IF EXISTS (SELECT FROM pg_opclass WHERE opcname = 'g0_untrusted_ops')
       OR EXISTS (SELECT FROM pg_opfamily WHERE opfname = 'g0_untrusted_ops') THEN
        RAISE EXCEPTION 'failed opclass creation left catalog members behind';
    END IF;
    CREATE OPERATOR FAMILY public.g0_empty_family USING pin;
    BEGIN
        ALTER OPERATOR FAMILY public.g0_empty_family USING pin ADD
            OPERATOR 1 pg_catalog.= (text, text);
        RAISE EXCEPTION 'G0 accepted unreviewed operator family members';
    EXCEPTION WHEN feature_not_supported THEN
        NULL;
    END;
    DROP OPERATOR FAMILY public.g0_empty_family USING pin;
END;
$test$;

PREPARE g0_diagnostic AS SELECT pin.abi_check();
EXECUTE g0_diagnostic;
EXECUTE g0_diagnostic;
DEALLOCATE g0_diagnostic;
DROP TABLE public.g0_documents;
DROP EXTENSION pin;
DO $test$
BEGIN
    IF EXISTS (SELECT FROM pg_am WHERE amname = 'pin') THEN
        RAISE EXCEPTION 'access method survived DROP EXTENSION';
    END IF;
END;
$test$;
CREATE EXTENSION pin;
SELECT pin.abi_check();
SELECT pin.build_stage();
