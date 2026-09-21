#!/usr/bin/env python3
"""Exercise G7 only in an explicitly disposable PostgreSQL 18.6 cluster.

PostgreSQL owns parallel bitmap scheduling, snapshots, DSM and worker cleanup.
This harness requires observed workers and exact results, not planner switches.
Official contracts and the unqualified performance gate: docs/g7-selective.md.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time
from typing import Any, Iterator

SCHEMA = 'pin_g7_qualification'
TABLE = f'{SCHEMA}.docs'
INDEX = 'docs_pin'
COMMON = """
SET statement_timeout = '120s';
SET lock_timeout = '5s';
SET jit = off;
SET pin.enable_count_fastpath = off;
SET pin.enable_count_vm = off;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET min_parallel_index_scan_size = 0;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
"""


def nodes(plan: dict[str, Any]) -> Iterator[dict[str, Any]]:
    """Walk only plan children, not worker instrumentation copies."""
    stack = [plan]
    while stack:
        node = stack.pop()
        yield node
        stack.extend(reversed(node.get('Plans', [])))


def require_bitmap(plan: dict[str, Any], parallel: bool, lossy: bool | None = None) -> dict[str, int]:
    """Reject serial fallback, absent workers, wrong indexes and missing rechecks."""
    tree = list(nodes(plan))
    heaps = [node for node in tree if node.get('Node Type') == 'Bitmap Heap Scan'
             and node.get('Relation Name') == 'docs']
    indexes = [node for node in tree if node.get('Node Type') == 'Bitmap Index Scan'
               and node.get('Index Name') == INDEX]
    gathers = [node for node in tree if node.get('Node Type') in ('Gather', 'Gather Merge')]
    if len(heaps) != 1 or len(indexes) != 1:
        raise RuntimeError('expected one Pin bitmap producer and one heap scan')
    if any(node.get('Custom Plan Provider') == 'PinCount' for node in tree):
        raise RuntimeError('custom count must not substitute for the native parallel path')
    heap = heaps[0]
    if not heap.get('Recheck Cond'):
        raise RuntimeError('bitmap heap recheck condition is missing')
    launched = sum(node.get('Workers Launched', 0) for node in gathers)
    if parallel:
        if not heap.get('Parallel Aware') or launched < 1:
            raise RuntimeError('parallel bitmap plan did not actually launch a worker')
    elif gathers or heap.get('Parallel Aware'):
        raise RuntimeError('serial reference unexpectedly used parallel execution')
    lossified = heap.get('Lossy Heap Blocks', 0)
    # PG18 show_tidbitmap_info emits local and worker block counters separately.
    lossified += sum(worker.get('Lossy Heap Blocks', 0) for worker in heap.get('Workers', []))
    if lossy is True and lossified == 0:
        raise RuntimeError('low-memory test did not exercise lossy heap rechecks')
    if lossy is False and lossified != 0:
        raise RuntimeError('high-memory reference unexpectedly became lossy')
    return {'workers_launched': launched, 'lossy_heap_blocks': lossified}


def settings(parallel: bool, memory: str = '64MB', leader: bool = True, sequential: bool = False) -> str:
    return (COMMON + f"SET max_parallel_workers_per_gather = {2 if parallel else 0};\n"
            + f"SET work_mem = '{memory}';\n"
            + f"SET parallel_leader_participation = {'on' if leader else 'off'};\n"
            + f"SET enable_seqscan = {'on' if sequential else 'off'};\n"
            + f"SET enable_bitmapscan = {'off' if sequential else 'on'};\n")


class Cluster:
    def __init__(self, psql: str, artifacts: Path):
        self.command = [psql, '-X', '--no-password', '-qAt', '-v', 'ON_ERROR_STOP=1']
        self.artifacts = artifacts
        artifacts.mkdir(parents=True, exist_ok=True)
        self.sequence = 0

    def run(self, sql: str, options: str = COMMON) -> subprocess.CompletedProcess[str]:
        self.sequence += 1
        stem = self.artifacts / f'{self.sequence:03d}'
        stem.with_suffix('.sql').write_text(options + sql)
        result = subprocess.run(self.command, input=options + sql, text=True,
                                capture_output=True, timeout=180, check=False)
        stem.with_suffix('.log').write_text(result.stdout + '\nSTDERR:\n' + result.stderr)
        if result.returncode:
            raise RuntimeError(f'psql failed; see {stem}.log\n{result.stderr}')
        return result

    def plan(self, sql: str, options: str) -> dict[str, Any]:
        result = self.run('EXPLAIN (ANALYZE, VERBOSE, BUFFERS, TIMING OFF, FORMAT JSON) ' + sql,
                          options)
        return json.loads(result.stdout)[0]['Plan']

    def ids(self, predicate: str, options: str) -> list[int]:
        # compare full multisets; count/checksum collisions cannot hide wrong rows.
        result = self.run(f'SELECT id FROM ONLY {TABLE} WHERE {predicate};', options)
        return sorted(int(line) for line in result.stdout.splitlines())

    def count(self, predicate: str, options: str) -> int:
        result = self.run(f'SELECT count(*) FROM ONLY {TABLE} WHERE {predicate};', options)
        return int(result.stdout)


def predicate(query: str, residual: bool = False) -> str:
    literal = query.replace("'", "''")
    result = f"body OPERATOR(pin.@@@) pin.parse_query('{literal}')"
    return result + (' AND keep AND id % 7 <> 0' if residual else '')


def insert_sql(start: int, stop: int) -> str:
    return f"""
INSERT INTO {TABLE}
SELECT i,
       CASE WHEN i % 17 = 0 THEN NULL WHEN i % 19 = 0 THEN ''
            ELSE 'common ' || CASE i % 6
                WHEN 0 THEN 'alpha beta' WHEN 1 THEN 'alpha zeta beta'
                WHEN 2 THEN 'beta alpha' WHEN 3 THEN 'gamma café'
                WHEN 4 THEN 'alpha alpha' ELSE 'delta' END END,
       i % 5 <> 0, repeat('x', 1000)
FROM generate_series({start}, {stop}) AS s(i);
"""


def matrix(cluster: Cluster) -> list[dict[str, Any]]:
    observations = []
    for query, residual in [('common', False), ('alpha AND beta', False),
                            ('"alpha beta"', False), ('NOT gamma', False),
                            ('common', True)]:
        where = predicate(query, residual)
        expected = cluster.ids(where, settings(False, sequential=True))
        if len(set(expected)) != len(expected):
            raise RuntimeError('fixture has duplicate primary keys')
        serial = settings(False)
        if cluster.ids(where, serial) != expected:
            raise RuntimeError(f'serial indexed multiset differs for {query}')
        count_sql = f'SELECT count(*) FROM ONLY {TABLE} WHERE {where};'
        require_bitmap(cluster.plan(count_sql, serial), parallel=False, lossy=False)
        for memory in ['64MB', '64kB']:
            for leader in [True, False]:
                options = settings(True, memory, leader)
                plan = cluster.plan(count_sql, options)
                # broad common-term scans must actually cross the lossification boundary.
                lossy = memory == '64kB' if query == 'common' and not residual else None
                metrics = require_bitmap(plan, parallel=True, lossy=lossy)
                if cluster.count(where, options) != len(expected):
                    raise RuntimeError(f'parallel count differs for {query}')
                row_sql = f'SELECT id FROM ONLY {TABLE} WHERE {where};'
                require_bitmap(cluster.plan(row_sql, options), parallel=True)
                if cluster.ids(where, options) != expected:
                    raise RuntimeError(f'parallel multiset differs for {query}')
                observations.append(dict(query=query, residual=residual, memory=memory,
                                         leader=leader, rows=len(expected), **metrics))
    return observations


def prepared_rescan(cluster: Cluster) -> None:
    queries = ['common', 'gamma', 'absent', 'common']
    expected = [cluster.count(predicate(query), settings(False, sequential=True)) for query in queries]
    prepare = (f'PREPARE g7(text) AS SELECT count(*) FROM ONLY {TABLE} '
               'WHERE body OPERATOR(pin.@@@) pin.parse_query($1);\n')
    options = settings(True) + 'SET plan_cache_mode = force_generic_plan;\n'
    sql = prepare + ''.join(f"EXECUTE g7('{query}');\n" for query in queries)
    actual = [int(value) for value in cluster.run(sql, options).stdout.splitlines()]
    if actual != expected:
        raise RuntimeError('generic prepared execution retained stale query state')
    result = cluster.run(prepare + "EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON) EXECUTE g7('common');",
                         options)
    require_bitmap(json.loads(result.stdout)[0]['Plan'], parallel=True)
    # a transaction's own writes prevent native parallel execution; ordinary scans remain exact.
    own_write = (f"BEGIN; INSERT INTO {TABLE} VALUES (1000000, 'ownwrite common', true, 'x');\n"
                 f"SELECT count(*) FROM {TABLE} WHERE {predicate('ownwrite')}; ROLLBACK;")
    if cluster.run(own_write, options).stdout.strip() != '1':
        raise RuntimeError('own-write fallback failed')


def require_pin_count(plan: dict[str, Any]) -> None:
    tree = list(nodes(plan))
    custom = [node for node in tree if node.get('Custom Plan Provider') == 'PinCount']
    if len(custom) != 1:
        raise RuntimeError('expected one PinCount custom node')


def paused_pin_worker(cluster: Cluster, stage: int, sql: str, app: str,
                      expected: str | None = None, terminate_worker: bool = False) -> str:
    blocker_app = f'{app}-blocker'
    blocker = subprocess.Popen(cluster.command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True,
                               env=dict(os.environ, PGAPPNAME=blocker_app))
    target = None
    try:
        assert blocker.stdin is not None
        blocker.stdin.write("SELECT pg_advisory_lock(180006, 4); SELECT pg_sleep(120);")
        blocker.stdin.close()
        blocker.stdin = None

        deadline = time.monotonic() + 15
        blocker_pid = None
        while time.monotonic() < deadline:
            value = cluster.run(
                "SELECT a.pid FROM pg_stat_activity a JOIN pg_locks l USING (pid) "
                f"WHERE a.application_name = '{blocker_app}' "
                "AND l.locktype = 'advisory' AND l.granted LIMIT 1;").stdout.strip()
            if value:
                blocker_pid = int(value)
                break
            time.sleep(0.05)
        if blocker_pid is None:
            raise RuntimeError(f'{app}: blocker did not acquire advisory lock')

        target = subprocess.Popen(cluster.command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True,
                                  env=dict(os.environ, PGAPPNAME=app))
        assert target.stdin is not None
        target.stdin.write(COMMON + f"SET pin.g7_pause_worker_stage = {stage};\n" + sql)
        target.stdin.close()
        target.stdin = None

        deadline = time.monotonic() + 20
        worker_pid = None
        while time.monotonic() < deadline and target.poll() is None:
            value = cluster.run(
                "SELECT w.pid FROM pg_stat_activity l "
                "JOIN pg_stat_activity w ON w.leader_pid = l.pid "
                "JOIN pg_locks k ON k.pid = w.pid "
                f"WHERE l.application_name = '{app}' "
                "AND l.backend_type = 'client backend' "
                "AND w.backend_type = 'parallel worker' "
                "AND k.locktype = 'advisory' AND NOT k.granted "
                "ORDER BY w.pid LIMIT 1;").stdout.strip()
            if value:
                worker_pid = int(value)
                break
            time.sleep(0.05)
        if worker_pid is None:
            raise RuntimeError(f'{app}: no paused Pin parallel worker observed')

        if terminate_worker:
            if cluster.run(f'SELECT pg_terminate_backend({worker_pid});').stdout.strip() != 't':
                raise RuntimeError(f'{app}: could not terminate Pin worker')
        if cluster.run(f'SELECT pg_terminate_backend({blocker_pid});').stdout.strip() != 't':
            raise RuntimeError(f'{app}: could not release blocker')

        stdout, stderr = target.communicate(timeout=30)
        (cluster.artifacts / f'{app}.log').write_text(stdout + '\nSTDERR:\n' + stderr)
        if terminate_worker:
            if target.returncode == 0:
                raise RuntimeError(f'{app}: worker failure reported query success')
        elif target.returncode != 0:
            raise RuntimeError(f'{app}: parallel operation failed\n{stderr}')
        elif expected is not None and stdout.strip() != expected:
            raise RuntimeError(f'{app}: expected {expected!r}, got {stdout.strip()!r}')
        return stdout
    finally:
        if target is not None and target.poll() is None:
            cluster.run("SELECT pg_terminate_backend(pid) FROM pg_stat_activity "
                        f"WHERE application_name = '{app}' AND backend_type = 'client backend';")
            try:
                target.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                target.kill()
                target.communicate(timeout=10)
        if blocker.poll() is None:
            cluster.run("SELECT pg_terminate_backend(pid) FROM pg_stat_activity "
                        f"WHERE application_name = '{blocker_app}' AND backend_type = 'client backend';")
            try:
                blocker.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                blocker.kill()
                blocker.communicate(timeout=10)


def parallel_count_qualification(cluster: Cluster) -> None:
    where = predicate('common')
    expected = cluster.count(where, settings(False, sequential=True))
    options = settings(True) + (
        'SET pin.enable_count_fastpath = on;\n'
        'SET pin.parallel_count_workers = 2;\n'
    )
    query = f'SELECT count(*) FROM ONLY {TABLE} WHERE {where};'
    require_pin_count(cluster.plan(query, options))

    sql = options + query
    paused_pin_worker(cluster, 17, sql, 'pin-g7-direct-count',
                      expected=str(expected))
    # stage 18 is after this worker successfully claimed one private batch.
    paused_pin_worker(cluster, 18, sql, 'pin-g7-direct-count-failure',
                      terminate_worker=True)

    if cluster.count(where, options) != expected:
        raise RuntimeError('PinCount result changed after worker termination')


def interrupt_workers(cluster: Cluster, terminate_worker: bool) -> None:
    app = 'pin-g7-worker-failure' if terminate_worker else 'pin-g7-leader-cancel'
    options = settings(True) + "SET statement_timeout = '60s';\n"
    # bounded by a deadline and cancelled as soon as a real parallel worker exists.
    query = (f'SELECT count(*) FROM ONLY {TABLE} d CROSS JOIN generate_series(1, 100000) s(i) '
             f"WHERE {predicate('common')};")
    process = subprocess.Popen(cluster.command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, env=dict(os.environ, PGAPPNAME=app))
    try:
        assert process.stdin is not None
        process.stdin.write(options + query)
        process.stdin.close()
        process.stdin = None
        deadline = time.monotonic() + 15
        target = None
        while time.monotonic() < deadline and process.poll() is None:
            rows = cluster.run(
                "SELECT l.pid, w.pid FROM pg_stat_activity l JOIN pg_stat_activity w "
                f"ON w.leader_pid = l.pid WHERE l.application_name = '{app}' "
                "AND l.backend_type = 'client backend' AND w.backend_type = 'parallel worker' "
                'ORDER BY w.pid LIMIT 1;').stdout.strip()
            if rows:
                leader, worker = (int(value) for value in rows.split('|'))
                target = worker if terminate_worker else leader
                break
            time.sleep(0.05)
        if target is None:
            raise RuntimeError(f'{app}: no active parallel worker observed')
        action = 'pg_terminate_backend' if terminate_worker else 'pg_cancel_backend'
        if cluster.run(f'SELECT {action}({target});').stdout.strip() != 't':
            raise RuntimeError(f'{app}: requested interruption failed')
        stdout, stderr = process.communicate(timeout=20)
        (cluster.artifacts / f'{app}.log').write_text(stdout + stderr)
        if process.returncode == 0:
            raise RuntimeError(f'{app}: incomplete parallel query was reported as success')
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            remaining = cluster.run(
                "SELECT count(*) FROM pg_stat_activity "
                f"WHERE application_name = '{app}';").stdout.strip()
            if remaining == '0':
                break
            time.sleep(0.05)
        else:
            raise RuntimeError(f'{app}: backend/worker resources outlived failed query')
        cluster.run(f'VACUUM (INDEX_CLEANUP ON) {TABLE};',
                    COMMON + "SET lock_timeout = '1s'; SET pin.enable_compact_reuse = on;\n")
        expected = cluster.count(predicate('common'), settings(False, sequential=True))
        if cluster.count(predicate('common'), settings(True)) != expected:
            raise RuntimeError('parallel execution after interruption changed results')
    finally:
        if process.poll() is None:
            try:
                cluster.run("SELECT pg_terminate_backend(pid) FROM pg_stat_activity "
                            f"WHERE application_name = '{app}' AND backend_type = 'client backend';")
            finally:
                try:
                    process.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate(timeout=10)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--psql', required=True)
    parser.add_argument('--artifacts', type=Path, required=True)
    parser.add_argument('--disposable', action='store_true', required=True)
    args = parser.parse_args()
    cluster = Cluster(args.psql, args.artifacts)
    if cluster.run('SHOW server_version_num;').stdout.strip() != '180006':
        raise RuntimeError('qualification requires PostgreSQL 18.6')
    if cluster.run('SHOW pin.enable_compact_reuse;').stdout.strip() != 'off':
        raise RuntimeError('compaction retention must default off')
    if cluster.run('SHOW pin.enable_parallel_vacuum;').stdout.strip() != 'on':
        raise RuntimeError('qualification requires parallel VACUUM enabled at postmaster start')
    # CREATE fails rather than dropping any pre-existing user schema.
    cluster.run(f'CREATE SCHEMA {SCHEMA};')
    try:
        cluster.run(f"""
CREATE TABLE {TABLE}(id integer PRIMARY KEY, body text, keep boolean NOT NULL, padding text)
WITH (parallel_workers = 2, fillfactor = 80, autovacuum_enabled = false);
{insert_sql(1, 16000)}
""")
        paused_pin_worker(
            cluster,
            16,
            f"SET maintenance_work_mem = '128MB'; CREATE INDEX {INDEX} ON {TABLE} USING pin(body);",
            'pin-g7-parallel-build',
        )
        # a killed build participant must abort CREATE INDEX without damaging the live index.
        paused_pin_worker(
            cluster,
            16,
            f"SET maintenance_work_mem = '128MB'; "
            f"CREATE INDEX pin_g7_build_failure_idx ON {TABLE} USING pin(body);",
            'pin-g7-parallel-build-failure',
            terminate_worker=True,
        )
        if cluster.run(
            f"SELECT to_regclass('{SCHEMA}.pin_g7_build_failure_idx') IS NULL;"
        ).stdout.strip() != 't':
            raise RuntimeError('failed parallel build left a catalog-visible index')

        cluster.run(f'VACUUM (ANALYZE, INDEX_CLEANUP ON) {TABLE};')
        cluster.run(insert_sql(16001, 17000))

        # two Pin indexes make a Pin worker assignment deterministic enough for
        # the disposable parallel-VACUUM lifecycle check; drop the shadow after.
        cluster.run(
            f"SET maintenance_work_mem = '128MB'; "
            f"CREATE INDEX pin_g7_vacuum_shadow_idx ON {TABLE} USING pin(body);"
        )
        cluster.run(
            f"INSERT INTO {TABLE} "
            "SELECT 50000 + g, 'vacuumdead common alpha', true, repeat('v', 1000) "
            "FROM generate_series(1, 64) g;"
        )
        cluster.run(f'DELETE FROM {TABLE} WHERE id BETWEEN 50001 AND 50064;')
        paused_pin_worker(
            cluster,
            7,
            f'VACUUM (PARALLEL 2, INDEX_CLEANUP ON) {TABLE};',
            'pin-g7-parallel-vacuum',
        )
        cluster.run(
            f"INSERT INTO {TABLE} "
            "SELECT 50100 + g, 'vacuumfail common alpha', true, repeat('w', 1000) "
            "FROM generate_series(1, 64) g;"
        )
        cluster.run(f'DELETE FROM {TABLE} WHERE id BETWEEN 50101 AND 50164;')
        paused_pin_worker(
            cluster,
            7,
            f'VACUUM (PARALLEL 2, INDEX_CLEANUP ON) {TABLE};',
            'pin-g7-parallel-vacuum-failure',
            terminate_worker=True,
        )
        cluster.run(f'VACUUM (PARALLEL 2, INDEX_CLEANUP ON) {TABLE};')
        cluster.run(f'DROP INDEX {SCHEMA}.pin_g7_vacuum_shadow_idx;')
        compacted = cluster.run(f'VACUUM (ANALYZE, INDEX_CLEANUP ON) {TABLE};',
                                COMMON + 'SET client_min_messages = debug1; '
                                'SET pin.enable_compact_reuse = on;\n')
        retained = [int(value) for value in re.findall(r'Pin compaction: retained_pages=(\d+)',
                                                       compacted.stderr)]
        if not retained or sum(retained) == 0:
            raise RuntimeError('host compaction did not exercise sealed-prefix retention')
        observations = matrix(cluster)
        parallel_count_qualification(cluster)
        prepared_rescan(cluster)
        interrupt_workers(cluster, terminate_worker=False)
        interrupt_workers(cluster, terminate_worker=True)
        # delete early/late owners, append replacements, then requalify the modified corpus.
        cluster.run(f'DELETE FROM {TABLE} WHERE id % 31 = 0;')
        cluster.run(f'VACUUM (INDEX_CLEANUP ON) {TABLE};',
                    COMMON + 'SET pin.enable_compact_reuse = on;\n')
        cluster.run(insert_sql(17001, 17300))
        cluster.run(f'VACUUM (ANALYZE, INDEX_CLEANUP ON) {TABLE};',
                    COMMON + 'SET pin.enable_compact_reuse = on;\n')
        where = predicate('common', residual=True)
        if cluster.ids(where, settings(True, '64kB', False)) != cluster.ids(
                where, settings(False, sequential=True)):
            raise RuntimeError('post-maintenance parallel multiset differs')
        (args.artifacts / 'observations.json').write_text(json.dumps(observations, indent=2) + '\n')
        print(f'G7: {len(observations)} parallel cases, prepared executions and worker cleanup passed')
        print('Qualification evidence only; no throughput, latency or total-memory claim.')
    finally:
        cluster.run(f'DROP SCHEMA {SCHEMA} CASCADE;')


if __name__ == '__main__':
    main()
