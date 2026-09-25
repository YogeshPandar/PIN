#!/usr/bin/env python3
"""Native qualification of opt-in grouped COUNT in an isolated PostgreSQL 18.6.

Normal mode tests exact row and visibility oracles and runtime fallback. Test-hook
mode additionally pauses the new generation guard and exercises contention,
ERROR, cancellation, termination, concurrent HOT/delete and immediate recovery.
No success or performance result is implied by the existence of this harness.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import time

try:
    from .g9_qualification import Cluster, literal, nodes
    from .g9_count_bench import body_sql
except ImportError:
    from g9_qualification import Cluster, literal, nodes
    from g9_count_bench import body_sql

TABLE = 'g9_count_docs'
BASE = """
SET enable_seqscan=off; SET enable_bitmapscan=on;
SET enable_indexscan=off; SET enable_indexonlyscan=off;
SET max_parallel_workers_per_gather=0; SET max_parallel_maintenance_workers=0;
SET pin.parallel_count_workers=0; SET jit=off;
SET pin.enable_count_fastpath=on; SET pin.enable_count_vm=on;
SET pin.enable_grouped_count=on; SET pin.enable_grouped_scan=on;
SET pin.enable_grouped_page_visibility=off;
SET pin.enable_exact_bitmap=on; SET work_mem='4MB';
SET maintenance_work_mem='4MB';
"""
CASES = (
    ('rareplanet', 'rareplanet'), ('alpha', 'alpha'),
    ('alpha AND bravo', 'alpha & bravo'), ('alpha OR bravo', 'alpha | bravo'),
    ('alpha AND NOT bravo', 'alpha & !bravo'), ('NOT alpha', '!alpha'),
    ('NOT missing', '!missing'), ('missing', 'missing'),
    ('"bravo charlie"', 'bravo <-> charlie'), ('alp*', 'alp:*'),
)


def predicate(pin: str, gin: str | None = None) -> str:
    if gin is not None:
        return f"to_tsvector('simple',body) @@ to_tsquery('simple',{literal(gin)})"
    return f'body OPERATOR(pin.@@@) pin.parse_query({literal(pin)})'


def count_sql(query: str = 'alpha AND bravo', table: str = TABLE) -> str:
    if table not in (TABLE, 'g9_count_reuse', 'g9_count_legacy'):
        raise ValueError('unexpected fixture table')
    return f'SELECT count(*) FROM ONLY {table} WHERE {predicate(query)};'


def custom(plan: list) -> list[dict]:
    return [n for n in nodes(plan[0]['Plan']) if n.get('Custom Plan Provider') == 'PinCount']


def verify(cluster: Cluster, label: str, *, table: str = TABLE,
           cases: tuple = CASES, require_grouped: bool = True) -> None:
    for pin, gin in cases:
        identities = (f"SELECT coalesce(json_agg(json_build_array(id,ctid::text) ORDER BY id,ctid),'[]'::json) "
                      f'FROM ONLY {table} WHERE ')
        oracle = json.loads(cluster.run(BASE + 'SET pin.enable_count_fastpath=off; '
            'SET enable_seqscan=on; SET enable_bitmapscan=off; ' + identities + predicate(pin, gin) + ';',
            label=label+'-pg-oracle'))
        for engine, where in (('pin', predicate(pin)), ('gin', predicate(pin, gin))):
            rows = json.loads(cluster.run(BASE + identities + where + ';', label=label+'-'+engine+'-ids'))
            if rows != oracle:
                raise AssertionError(f'{label}/{pin}: full row identity oracle mismatch for {engine}')
        for vm in ('off', 'on'):
            for batch in ('off', 'on'):
                settings = BASE + (f'SET pin.enable_count_vm={vm}; '
                                   f'SET pin.enable_grouped_page_visibility={batch}; ')
                suffix = vm+'-'+batch
                result = cluster.run(settings + count_sql(pin, table), label=label+'-count-'+suffix)
                if int(result) != len(oracle):
                    raise AssertionError(f'{label}/{pin}/{suffix}: visible count differs from PG identities')
                plan = json.loads(cluster.run(settings + 'EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) '
                                              + count_sql(pin, table), label=label+'-plan-'+suffix))
                selected = custom(plan)
                if pin.startswith('"') or pin.endswith('*'):
                    if selected:
                        raise AssertionError('phrase/prefix must retain the ordinary executor')
                elif require_grouped and pin == 'alpha AND bravo':
                    if len(selected) != 1 or selected[0].get('Grouped Count Runs') != 1:
                        raise AssertionError('expected actual grouped COUNT execution, not GUC enablement')
                    if vm == 'off' and selected[0].get('VM Certified Roots') != 0:
                        raise AssertionError('VM-off query elided heap checks')


def fixtures(cluster: Cluster) -> None:
    cluster.run('CREATE EXTENSION IF NOT EXISTS pin;')
    defaults = cluster.run("SELECT current_setting('pin.enable_grouped_count');")
    if defaults != 'off':
        raise AssertionError('experimental grouped count must default off')
    cluster.run(BASE + f"""
        CREATE TABLE {TABLE}(id bigint,body text,notes integer DEFAULT 0)
            WITH (autovacuum_enabled=false,fillfactor=50);
        INSERT INTO {TABLE}(id,body) SELECT g,{body_sql()} FROM generate_series(1,1200) g;
        INSERT INTO {TABLE}(id,body) VALUES (1201,''),(1202,NULL);
        SET pin.enable_grouped_storage=on;
        CREATE INDEX g9_count_pin ON {TABLE} USING pin(body);
        CREATE INDEX g9_count_gin ON {TABLE} USING gin(to_tsvector('simple',body));
        VACUUM (ANALYZE,INDEX_CLEANUP ON,PARALLEL 0) {TABLE};
    """, label='fresh')
    verify(cluster, 'fresh')
    for lo, hi, name in ((1300, 1316, 'short'), (1400, 2010, 'long')):
        cluster.run(BASE + f'INSERT INTO {TABLE}(id,body) SELECT g,{body_sql(rare=False)} '
                    f'FROM generate_series({lo},{hi}) g;', label=name+'-delta')
        verify(cluster, name)
    # an unrelated-column update must really create a HOT version in this fixture.
    hot = cluster.run(BASE + f"BEGIN; UPDATE {TABLE} SET notes=notes+1 WHERE id<=10; "
          f"SELECT n_tup_hot_upd FROM pg_stat_xact_user_tables WHERE relid='{TABLE}'::regclass; COMMIT;",
          label='hot-update')
    if int(hot) <= 0:
        raise AssertionError('fixture failed to make a HOT chain')
    cluster.run(f"UPDATE {TABLE} SET body='bravo replacement' WHERE id%17=0; "
                f"DELETE FROM {TABLE} WHERE id%19=0;", label='update-delete')
    verify(cluster, 'dirty-visibility')
    own = cluster.run(BASE + f"BEGIN; INSERT INTO {TABLE}(id,body) VALUES (3000,'alpha bravo'); "
           + count_sql() + 'SET pin.enable_count_fastpath=off; SET enable_seqscan=on; SET enable_bitmapscan=off; '
           + f"SELECT count(*) FROM {TABLE} WHERE {predicate('', 'alpha & bravo')}; ROLLBACK;", label='own-write')
    values = [int(x) for x in own.splitlines() if x.strip()]
    if len(values) != 2 or values[0] != values[1]:
        raise AssertionError('own-write snapshot mismatch')
    cluster.run(BASE + 'SET pin.enable_grouped_storage=on; '
                f'VACUUM (ANALYZE,INDEX_CLEANUP ON,PARALLEL 0) {TABLE};', label='rebuilt')
    verify(cluster, 'rebuilt')


def fallbacks(cluster: Cluster) -> None:
    # a cached CustomScan must recheck runtime gates, not retain planning privileges.
    result = cluster.run(BASE + 'SET plan_cache_mode=force_generic_plan; PREPARE gc AS '
        + count_sql() + 'EXECUTE gc; SET pin.enable_grouped_count=off; EXECUTE gc; '
        'SET pin.enable_grouped_count=on; SET work_mem=\'64kB\'; EXECUTE gc;', label='cached-fallback')
    values = [int(x) for x in result.splitlines() if x.strip()]
    if len(values) != 3 or len(set(values)) != 1:
        raise AssertionError('cached runtime or memory fallback changed the result')
    for suffix in ('', ' AND id>0'):
        query = count_sql().rstrip(';') + suffix + ';'
        prefix = BASE + ('BEGIN ISOLATION LEVEL SERIALIZABLE;' if not suffix else '')
        plan = json.loads(cluster.run(prefix + 'EXPLAIN (FORMAT JSON) ' + query,
                                      label='ineligible-plan'))
        if custom(plan):
            raise AssertionError('serializable/residual predicate entered fast path')
    cluster.run(BASE + "SET pin.enable_grouped_storage=off; "
        'CREATE TABLE g9_count_legacy(id bigint,body text); '
        "INSERT INTO g9_count_legacy VALUES(1,'alpha bravo'); "
        'CREATE INDEX ON g9_count_legacy USING pin(body);', label='legacy-format')
    verify(cluster, 'legacy-fallback', table='g9_count_legacy',
           cases=(('alpha AND bravo','alpha & bravo'),), require_grouped=False)



def security_and_shapes(cluster: Cluster) -> None:
    where = predicate('alpha AND bravo')
    for expression in ('count(id)', 'count(DISTINCT id)', 'count(*) FILTER (WHERE id>0)',
                       'count(*),max(id)'):
        plan = json.loads(cluster.run(BASE + f'EXPLAIN (FORMAT JSON) SELECT {expression} '
                                      f'FROM ONLY {TABLE} WHERE {where};', label='aggregate-shape'))
        if custom(plan):
            raise AssertionError('unsupported aggregate shape entered grouped count')
    cluster.run(f'CREATE ROLE gc_reader NOLOGIN; GRANT SELECT ON {TABLE} TO gc_reader; '
                'GRANT USAGE ON SCHEMA pin TO gc_reader; '
                f'ALTER TABLE {TABLE} ENABLE ROW LEVEL SECURITY; '
                f'CREATE POLICY gc_even ON {TABLE} USING (id%2=0);', label='rls-setup')
    plan = json.loads(cluster.run(BASE + 'SET ROLE gc_reader; EXPLAIN (FORMAT JSON) ' + count_sql(),
                                  label='rls-plan'))
    if custom(plan):
        raise AssertionError('row security must exclude custom count')
    result = cluster.run(BASE + 'SET ROLE gc_reader; ' + count_sql()
        + f"SELECT count(*) FROM ONLY {TABLE} WHERE {predicate('', 'alpha & bravo')};", label='rls-oracle')
    values = [x for x in result.splitlines() if x.strip()]
    if len(values) != 2 or values[0] != values[1]:
        raise AssertionError('RLS changed row identity semantics between predicates')
    cluster.run('SET ROLE gc_reader; SET pin.enable_grouped_count=on;',
                error='permission denied', label='gate-permission')
    cluster.run(f'ALTER TABLE {TABLE} DISABLE ROW LEVEL SECURITY;', label='rls-reset')


def reuse(cluster: Cluster) -> None:
    cluster.run(BASE + """
        SET pin.enable_grouped_storage=on;
        CREATE TABLE g9_count_reuse(id bigint,body text,notes integer)
            WITH (autovacuum_enabled=false,fillfactor=50);
        CREATE INDEX ON g9_count_reuse USING pin(body);
        INSERT INTO g9_count_reuse VALUES(1,'alpha',0),(2,'alpha bravo',0);
        INSERT INTO g9_count_reuse SELECT i,'filler',0 FROM generate_series(10,73) i;
        CREATE INDEX ON g9_count_reuse USING gin(to_tsvector('simple',body));
        VACUUM (INDEX_CLEANUP ON,PARALLEL 0) g9_count_reuse;
        CREATE TEMP TABLE gc_old AS SELECT ctid old_tid FROM g9_count_reuse WHERE id=1;
        DELETE FROM g9_count_reuse WHERE id=1;
        SET pin.enable_grouped_storage=off; SET pin.enable_grouped_scan=off;
        VACUUM (INDEX_CLEANUP ON,PARALLEL 0) g9_count_reuse;
        INSERT INTO g9_count_reuse VALUES(3,'bravo',0);
        DO $$ BEGIN
          IF NOT EXISTS (SELECT FROM g9_count_reuse n,gc_old o WHERE n.id=3 AND n.ctid=o.old_tid)
          THEN RAISE EXCEPTION 'test did not reuse the retired heap slot'; END IF;
        END $$;
    """, label='actual-tid-reuse')
    verify(cluster, 'reused', table='g9_count_reuse', cases=(('alpha AND bravo','alpha & bravo'),))
    cluster.run(BASE + 'SET pin.enable_grouped_storage=on; '
                'VACUUM (INDEX_CLEANUP ON,PARALLEL 0) g9_count_reuse;', label='reuse-rebuilt')
    verify(cluster, 'reused-rebuilt', table='g9_count_reuse', cases=(('alpha AND bravo','alpha & bravo'),))


def wait_writer(cluster: Cluster, app: str) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        got = cluster.run(f"SELECT EXISTS (SELECT FROM pg_locks l JOIN pg_stat_activity a USING(pid) "
            f"WHERE a.application_name={literal(app)} AND l.locktype='page' AND l.page=0 "
            "AND l.mode='ExclusiveLock' AND NOT l.granted);", label='writer-wait')
        if got == 't':
            return
        time.sleep(.05)
    raise AssertionError('writer did not wait on the generation interlock')


def concurrency(cluster: Cluster) -> None:
    expected = cluster.run(BASE + count_sql())
    blocker = cluster.blocker('gc-count-blocker')
    reader = cluster.start(BASE + 'SELECT pin.g2_inject(42,1,true); '+count_sql(), 'gc-count-reader')
    cluster.wait_lock('gc-count-reader', False)
    writer = cluster.start(f"INSERT INTO {TABLE}(id,body) VALUES(4000,'alpha bravo');", 'gc-count-writer')
    wait_writer(cluster, 'gc-count-writer')
    # another reader stays correct; a queued writer may force its ordinary fallback.
    if cluster.run(BASE + count_sql(), label='concurrent-reader') != expected:
        raise AssertionError('concurrent reader saw an uncommitted owner')
    cluster.release('gc-count-blocker', blocker)
    if cluster.finish(reader) != expected:
        raise AssertionError('protected reader changed snapshot membership')
    cluster.finish(writer)
    verify(cluster, 'after-writer', cases=(('alpha AND bravo','alpha & bravo'),))

    # an older repeatable-read snapshot remains valid while HOT/update/delete and
    # VACUUM occur between two separate protected executions.
    expected = cluster.run(BASE + count_sql())
    blocker = cluster.blocker('gc-rr-blocker')
    reader = cluster.start(BASE+'BEGIN ISOLATION LEVEL REPEATABLE READ; '+count_sql()
        +'SELECT pg_advisory_xact_lock(180006,2); '+count_sql()+'COMMIT;', 'gc-rr-reader')
    cluster.wait_lock('gc-rr-reader', False)
    cluster.run(f"UPDATE {TABLE} SET notes=notes+1 WHERE id=2; DELETE FROM {TABLE} WHERE id=4000;")
    cluster.run(BASE+f'SET pin.enable_grouped_storage=on; VACUUM (INDEX_CLEANUP ON,PARALLEL 0) {TABLE};')
    cluster.release('gc-rr-blocker', blocker)
    values = [x for x in cluster.finish(reader).splitlines() if x.strip()]
    if values != [expected, expected]:
        raise AssertionError('old MVCC snapshot changed across vacuum')



def writer_contention(cluster: Cluster) -> None:
    expected = cluster.run(BASE + count_sql())
    blocker = cluster.blocker('gc-owner-blocker')
    writer = cluster.start('SELECT pin.g2_inject(2,1,true); '
        + f"INSERT INTO {TABLE}(id,body) VALUES(4100,'alpha bravo');", 'gc-owner-writer')
    cluster.wait_lock('gc-owner-writer', False)
    plan = json.loads(cluster.run(BASE + "SET statement_timeout='5s'; "
        + 'EXPLAIN (ANALYZE,FORMAT JSON) ' + count_sql(), label='writer-contention-plan'))
    selected = custom(plan)
    if len(selected) != 1 or 'writer contention' not in selected[0].get('Fallback', ''):
        raise AssertionError('failed conditional guard did not retain the real core plan')
    if cluster.run(BASE + "SET statement_timeout='5s'; " + count_sql()) != expected:
        raise AssertionError('contention fallback saw an uncommitted row')
    cluster.release('gc-owner-blocker', blocker)
    cluster.finish(writer)


def failures(cluster: Cluster) -> None:
    cluster.run(BASE+'SELECT pin.g2_inject(42,1,false); '+count_sql(),
                error='Pin injected storage error', label='guard-error')
    for stage, hit in ((42, 1), (15, 2)):
        # the second visibility decision tests cancellation after partial work.
        for signal, expected in (('pg_cancel_backend', 'canceling statement due to user request'),
                                 ('pg_terminate_backend', 'terminating connection')):
            blocker = cluster.blocker('gc-failure-blocker')
            reader = cluster.start(BASE+f'SELECT pin.g2_inject({stage},{hit},true); '+count_sql(),
                                   'gc-failure-reader')
            cluster.wait_lock('gc-failure-reader', False)
            cluster.signal('gc-failure-reader', signal)
            cluster.finish(reader, error=expected)
            cluster.release('gc-failure-blocker', blocker)
            # timeout makes a leaked heavyweight lock a hard failure.
            cluster.run(f"SET statement_timeout='5s'; INSERT INTO {TABLE}(id,body) VALUES(5000,'bravo'); "
                        f'DELETE FROM {TABLE} WHERE id=5000;', label='post-error-writer')
    cluster.run('CREATE TABLE gc_witness(id integer); INSERT INTO gc_witness VALUES(1); CHECKPOINT; '
                f"INSERT INTO {TABLE}(id,body) VALUES(7000,'alpha bravo recovery'); "
                'INSERT INTO gc_witness VALUES(2);')
    blocker = cluster.blocker('gc-crash-blocker')
    reader = cluster.start(BASE+'SELECT pin.g2_inject(42,1,true); '+count_sql(), 'gc-crash-reader')
    cluster.wait_lock('gc-crash-reader', False)
    cluster.restart(immediate=True)
    reader[0].wait(timeout=20); blocker[0].wait(timeout=20)
    if cluster.run('SELECT count(*) FROM gc_witness;') != '2':
        raise AssertionError('durable witness missing after recovery')
    verify(cluster, 'crash-recovered', cases=(('alpha AND bravo','alpha & bravo'),))
    cluster.run(f"SET statement_timeout='5s'; INSERT INTO {TABLE}(id,body) VALUES(6000,'alpha bravo');",
                label='post-crash-writer')


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--psql', required=True); p.add_argument('--pg-ctl', required=True)
    p.add_argument('--data', type=Path, required=True); p.add_argument('--server-log', type=Path, required=True)
    p.add_argument('--artifacts', type=Path, required=True)
    p.add_argument('--hooks', choices=('0','1'), default='0')
    p.add_argument('--anchors', choices=('0','1'), default='0')
    p.add_argument('--owner-frontier', choices=('0','1'), default='0')
    args = p.parse_args(); cluster = Cluster(args)
    status = {'status':'running','native':True,'hooks':args.hooks}
    try:
        fixtures(cluster); fallbacks(cluster); security_and_shapes(cluster); reuse(cluster)
        if args.hooks == '1':
            concurrency(cluster); writer_contention(cluster); failures(cluster)
        else:
            cluster.restart(immediate=True)
            verify(cluster, 'restart', cases=(('alpha AND bravo','alpha & bravo'),))
        status['status'] = 'passed'
    except BaseException as error:
        status.update(status='failed',error=repr(error)); raise
    finally:
        cluster.cleanup()
        (args.artifacts/'count-status.json').write_text(json.dumps(status,indent=2)+'\n')


if __name__ == '__main__':
    main()
