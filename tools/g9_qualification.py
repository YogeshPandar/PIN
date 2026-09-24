#!/usr/bin/env python3
"""Qualify grouped storage against heap scans in a disposable PostgreSQL cluster."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
STAGES = (32, 33, 34, 35, 36, 38)
QUERIES = (
    'alpha', 'beta', 'alpha AND beta', 'alpha AND NOT beta',
    'alpha OR beta', 'NOT alpha', 'NOT missing',
    '(alpha AND beta) OR delta', 'alpha*', '"alpha beta"',
)
SETTINGS = """
SET pin.enable_count_fastpath = off;
SET pin.enable_count_vm = off;
SET pin.enable_grouped_scan = on;
SET maintenance_work_mem = '4MB';
SET max_parallel_maintenance_workers = 0;
SET max_parallel_workers_per_gather = 0;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
"""


def literal(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def identifier(value: str) -> str:
    if re.fullmatch(r'g9_[a-z_]+', value) is None:
        raise ValueError('unexpected fixture identifier')
    return value


def select_rows(table: str, query: str) -> str:
    return (
        f"SELECT COALESCE(json_agg(id ORDER BY id), '[]'::json) FROM {identifier(table)} "
        f"WHERE body OPERATOR(pin.@@@) pin.parse_query({literal(query)})"
    )


def nodes(plan: dict[str, Any]):
    yield plan
    for child in plan.get('Plans', []):
        yield from nodes(child)


def require_bitmap(plan: list[dict[str, Any]]) -> None:
    if not any(node.get('Node Type') == 'Bitmap Index Scan' for node in nodes(plan[0]['Plan'])):
        raise AssertionError(f'expected a bitmap index plan: {plan}')


class Cluster:
    def __init__(self, args: argparse.Namespace):
        self.args = args
        self.anchors = getattr(args, 'anchors', '0') == '1'
        self.owner_frontier = getattr(args, 'owner_frontier', '0') == '1'
        self.sequence = 0
        self.children: list[subprocess.Popen] = []
        self.args.artifacts.mkdir(parents=True, exist_ok=True)

    def command(self) -> list[str]:
        return [self.args.psql, '-X', '-q', '-A', '-t', '-v', 'ON_ERROR_STOP=1']

    def paths(self, label: str) -> tuple[Path, Path]:
        self.sequence += 1
        name = f'{self.sequence:03d}-{label}'
        return self.args.artifacts / (name + '.sql'), self.args.artifacts / (name + '.log')

    def settings(self, sql: str) -> str:
        return ('SET pin.enable_frontier_anchors = '
                + ('on' if self.anchors else 'off') + ';\n'
                + 'SET pin.enable_owner_frontier = '
                + ('on' if self.owner_frontier else 'off') + ';\n' + sql)

    def run(self, sql: str, *, error: str | None = None, label: str = 'query') -> str:
        sql = self.settings(sql)
        source, log = self.paths(label)
        source.write_text(sql + '\n', encoding='utf-8')
        result = subprocess.run(
            self.command(), input=sql + '\n', text=True, capture_output=True,
            timeout=200, check=False,
        )
        log.write_text(result.stdout + result.stderr, encoding='utf-8')
        if error is not None:
            if result.returncode == 0 or error not in result.stderr:
                raise AssertionError(f'expected {error!r}; inspect {log}')
        elif result.returncode != 0:
            raise AssertionError(f'psql exited {result.returncode}; inspect {log}')
        return result.stdout.strip()

    def start(self, sql: str, app: str) -> tuple[subprocess.Popen, Path]:
        sql = self.settings(sql)
        source, log = self.paths(app)
        source.write_text(sql + '\n', encoding='utf-8')
        env = dict(os.environ, PGAPPNAME=app)
        with source.open('r', encoding='utf-8') as stream, log.open('w', encoding='utf-8') as output:
            process = subprocess.Popen(
                self.command(), stdin=stream, stdout=output, stderr=subprocess.STDOUT,
                env=env, text=True,
            )
        self.children.append(process)
        return process, log

    @staticmethod
    def finish(child: tuple[subprocess.Popen, Path], *, error: str | None = None) -> str:
        process, log = child
        status = process.wait(timeout=200)
        output = log.read_text(encoding='utf-8')
        if error is not None:
            if status == 0 or error not in output:
                raise AssertionError(f'expected {error!r}; inspect {log}')
        elif status != 0:
            raise AssertionError(f'psql exited {status}; inspect {log}')
        return output.strip()

    def wait_lock(self, app: str, granted: bool) -> None:
        sql = f"""SELECT EXISTS (
            SELECT FROM pg_stat_activity a JOIN pg_locks l USING (pid)
            WHERE a.application_name = {literal(app)} AND l.locktype = 'advisory'
              AND l.granted = {'true' if granted else 'false'})"""
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.run(sql, label='lock') == 't':
                return
            time.sleep(0.05)
        raise AssertionError(f'backend did not reach expected advisory lock: {app}')

    def wait_structure_lock(self, app: str) -> None:
        sql = f"""SELECT EXISTS (
            SELECT FROM pg_stat_activity a JOIN pg_locks l USING (pid)
            WHERE a.application_name = {literal(app)} AND l.locktype = 'page'
              AND l.page = 1 AND l.mode = 'ExclusiveLock' AND NOT l.granted)"""
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.run(sql, label='structure-lock') == 't':
                return
            time.sleep(0.05)
        raise AssertionError(f'backend did not wait for the structural page lock: {app}')

    def signal(self, app: str, function: str) -> None:
        if function not in ('pg_cancel_backend', 'pg_terminate_backend'):
            raise ValueError('unexpected backend signal')
        result = self.run(
            f"SELECT {function}(pid) FROM pg_stat_activity "
            f"WHERE application_name = {literal(app)}", label='signal',
        )
        if result != 't':
            raise AssertionError(f'expected one live backend: {app}: {result}')

    def blocker(self, app: str) -> tuple[subprocess.Popen, Path]:
        child = self.start('SELECT pg_advisory_lock(180006, 2); SELECT pg_sleep(170);', app)
        self.wait_lock(app, True)
        return child

    def release(self, app: str, child: tuple[subprocess.Popen, Path]) -> None:
        self.signal(app, 'pg_terminate_backend')
        child[0].wait(timeout=20)

    def restart(self, *, immediate: bool) -> None:
        _, log = self.paths('restart')
        with log.open('w', encoding='utf-8') as output:
            for tail in (
                ['-m', 'immediate' if immediate else 'fast', '-w', 'stop'],
                ['-l', str(self.args.server_log), '-w', 'start'],
            ):
                subprocess.run(
                    [self.args.pg_ctl, '-D', str(self.args.data), *tail],
                    stdout=output, stderr=subprocess.STDOUT, timeout=120, check=True,
                )

    def compare(self, table: str, queries: tuple[str, ...] = QUERIES) -> None:
        for query in queries:
            statement = select_rows(table, query)
            expected = json.loads(self.run(
                SETTINGS + 'SET enable_seqscan = on; SET enable_bitmapscan = off;\n' + statement,
                label='heap-oracle',
            ))
            for gate in ('off', 'on'):
                prefix = SETTINGS + (
                    f'SET pin.enable_grouped_scan = {gate};\n'
                    'SET enable_seqscan = off; SET enable_bitmapscan = on;\n'
                )
                require_bitmap(json.loads(self.run(prefix + 'EXPLAIN (FORMAT JSON) ' + statement)))
                actual = json.loads(self.run(prefix + statement, label='bitmap-oracle'))
                if actual != expected:
                    raise AssertionError(f'{table}: {gate}: {query}: {actual} != {expected}')

    def fixture(self) -> None:
        self.run(SETTINGS + """
            SET pin.enable_grouped_storage = on;
            DROP TABLE IF EXISTS g9_crash;
            CREATE TABLE g9_crash(id integer, body text) WITH (autovacuum_enabled = false);
            INSERT INTO g9_crash VALUES (1, 'alpha'), (2, 'beta'), (3, 'alpha beta');
            CREATE INDEX g9_crash_pin ON g9_crash USING pin(body);
            INSERT INTO g9_crash VALUES (4, 'alpha beta delta');
            DELETE FROM g9_crash WHERE id = 1;
            CHECKPOINT;
        """, label='crash-fixture')

    def vacuum(self, *, hook: int | None = None, pause: bool = False) -> str:
        inject = '' if hook is None else f'SELECT pin.g2_inject({hook}, 1, {str(pause).lower()});\n'
        return SETTINGS + 'SET pin.enable_grouped_storage = on;\n' + inject + (
            'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_crash;'
        )

    def cleanup(self) -> None:
        for child in self.children:
            if child.poll() is None:
                child.terminate()
        for child in self.children:
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)


def prove_scan_selection(cluster: Cluster) -> None:
    # each session consumes or discards its one-shot hook independently.
    statement = select_rows('g9_small', 'beta AND NOT missingplanet')
    prefix = SETTINGS + 'SET enable_seqscan = off; SET enable_bitmapscan = on;\n'
    cluster.run(prefix + 'SELECT pin.g2_inject(37, 1, false);\n' + statement,
                error='Pin injected storage error', label='prove-grouped-scan')
    cluster.run(prefix + 'SET pin.enable_grouped_scan = off;\n'
                'SELECT pin.g2_inject(37, 1, false);\n' + statement,
                label='prove-gated-fallback')
    cluster.run(prefix + 'SELECT pin.g2_inject(37, 1, false);\n'
                + select_rows('g9_small', 'beta'), label='prove-sparse-fallback')
    cluster.run(prefix + 'SELECT pin.g2_inject(37, 1, false);\n'
                + select_rows('g9_small', '"alpha beta"'), label='prove-phrase-fallback')


def crash_boundaries(cluster: Cluster) -> None:
    cluster.run('CREATE TABLE g9_wal_witness(stage integer PRIMARY KEY);')
    for stage in (*STAGES, 39) if cluster.anchors else STAGES:
        cluster.fixture()
        cluster.run(cluster.vacuum(hook=stage), error='Pin injected storage error',
                    label=f'error-{stage}')
        cluster.compare('g9_crash')
        cluster.run(cluster.vacuum(), label=f'error-recovery-{stage}')
        cluster.compare('g9_crash', ('alpha AND beta', 'NOT alpha'))

        cluster.fixture()
        blocker_app = f'g9-blocker-{stage}'
        paused_app = f'g9-crash-{stage}'
        blocker = cluster.blocker(blocker_app)
        paused = cluster.start(cluster.vacuum(hook=stage, pause=True), paused_app)
        cluster.wait_lock(paused_app, False)
        # this synchronous heap-only commit flushes the preceding paused index wal.
        cluster.run(f'SET synchronous_commit = on; INSERT INTO g9_wal_witness VALUES ({stage});',
                    label=f'wal-witness-{stage}')
        cluster.restart(immediate=True)
        paused[0].wait(timeout=20)
        blocker[0].wait(timeout=20)
        if cluster.run(f'SELECT count(*) FROM g9_wal_witness WHERE stage = {stage}') != '1':
            raise AssertionError('post-crash witness was not durable')
        cluster.compare('g9_crash')
        # retirement and recovery cannot depend on the experiment remaining enabled.
        cluster.run('SET pin.enable_frontier_anchors = off;\n'
                    'SET pin.enable_grouped_storage = off; SET pin.enable_grouped_scan = off;\n'
                    'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_crash;', label='disabled-recovery')
        cluster.run(cluster.vacuum(), label=f'crash-recovery-{stage}')
        cluster.compare('g9_crash')


def cancellation(cluster: Cluster) -> None:
    # the large lifecycle fixture leaves too little sort memory to avoid spill.
    cluster.run("INSERT INTO g9_docs VALUES (14000, 'alpha beta spillcancel', repeat('s', 1800));")
    app = 'g9-cancel'
    blocker = cluster.blocker('g9-cancel-blocker')
    paused = cluster.start(SETTINGS + 'SET pin.enable_grouped_storage = on;\n'
                           'SELECT pin.g2_inject(38, 1, true);\n'
                           'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_docs;', app)
    cluster.wait_lock(app, False)
    if cluster.run('SELECT EXISTS (SELECT FROM pg_ls_tmpdir())', label='live-spill') != 't':
        raise AssertionError('cancellation did not reach a spilled grouped sort')
    cluster.signal(app, 'pg_cancel_backend')
    cluster.finish(paused, error='canceling statement due to user request')
    cluster.release('g9-cancel-blocker', blocker)
    if cluster.run('SELECT count(*) FROM pg_ls_tmpdir()', label='spill-cleanup') != '0':
        raise AssertionError('canceled grouped sort left a temporary file')
    cluster.compare('g9_docs', ('alpha AND beta', 'spillcancel', 'NOT alpha'))
    cluster.run(SETTINGS + 'SET pin.enable_grouped_storage = on;\n'
                'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_docs;', label='cancel-recovery')
    cluster.compare('g9_docs', ('alpha AND beta', 'spillcancel'))


def concurrent_reader_writer_maintenance(cluster: Cluster) -> None:
    cluster.fixture()
    query = 'alpha AND NOT missingplanet'
    # the reader snapshot predates both the insert and replacement publication.
    expected = json.loads(cluster.run(SETTINGS + select_rows('g9_crash', query)))
    blocker = cluster.blocker('g9-reader-blocker')
    reader = cluster.start(SETTINGS + 'SET enable_seqscan = off; SET enable_bitmapscan = on;\n'
                           'SELECT pin.g2_inject(37, 1, true);\n'
                           + select_rows('g9_crash', query), 'g9-reader')
    cluster.wait_lock('g9-reader', False)
    cluster.run("SET statement_timeout = '10s'; INSERT INTO g9_crash VALUES (5, 'alpha');",
                label='writer-during-grouped-read')
    maintenance = cluster.start(cluster.vacuum(), 'g9-maintenance')
    cluster.wait_structure_lock('g9-maintenance')
    cluster.release('g9-reader-blocker', blocker)
    if json.loads(cluster.finish(reader)) != expected:
        raise AssertionError('reader observed a later tuple generation')
    cluster.finish(maintenance)
    cluster.compare('g9_crash')

    # repeatable read retains old heap versions across grouped rebuild and vacuum.
    expected = json.loads(cluster.run(SETTINGS + select_rows('g9_crash', query)))
    blocker = cluster.blocker('g9-snapshot-blocker')
    reader = cluster.start(SETTINGS + 'SET enable_seqscan = off; SET enable_bitmapscan = on;\n'
                           'BEGIN ISOLATION LEVEL REPEATABLE READ;\n'
                           + select_rows('g9_crash', query) + ';\n'
                           'SELECT pg_advisory_xact_lock(180006, 2);\n'
                           + select_rows('g9_crash', query) + '; COMMIT;', 'g9-snapshot')
    cluster.wait_lock('g9-snapshot', False)
    cluster.run("UPDATE g9_crash SET body = 'beta replacement' WHERE id = 3;")
    cluster.run(cluster.vacuum(), label='snapshot-vacuum')
    cluster.release('g9-snapshot-blocker', blocker)
    results = [json.loads(line) for line in cluster.finish(reader).splitlines() if line.strip()]
    if results != [expected, expected]:
        raise AssertionError(f'repeatable-read mismatch: {results} != {expected}')
    cluster.run(cluster.vacuum(), label='snapshot-final-vacuum')
    cluster.compare('g9_crash')


def owner_frontier_qualification(cluster: Cluster, hooks: bool) -> None:
    cluster.run(SETTINGS + """
        DROP TABLE IF EXISTS g9_owner_frontier;
        CREATE TABLE g9_owner_frontier(id bigint PRIMARY KEY, body text, marker integer)
            WITH (autovacuum_enabled = false, fillfactor = 50);
        INSERT INTO g9_owner_frontier
            SELECT i, CASE WHEN i <= 20 THEN 'alpha beta rareplanet'
                           ELSE 'alpha beta' END, 0
            FROM generate_series(1, 1024) AS i;
        SET pin.enable_grouped_storage = on;
        CREATE INDEX g9_owner_frontier_pin ON g9_owner_frontier USING pin(body);
        SET pin.enable_grouped_storage = off;
        INSERT INTO g9_owner_frontier
            SELECT i, CASE WHEN i % 3 = 0 THEN 'alpha beta rareplanet'
                           ELSE 'alpha rareplanet' END, 0
            FROM generate_series(1025, 2048) AS i;
        UPDATE g9_owner_frontier SET marker = marker + 1 WHERE id <= 10;
        UPDATE g9_owner_frontier SET body = 'beta replacement' WHERE id = 11;
        DELETE FROM g9_owner_frontier WHERE id = 12;
    """, label='owner-frontier-fixture')
    queries = ('alpha AND rareplanet', 'alpha OR rareplanet',
               'alpha AND NOT rareplanet', 'NOT missing')
    cluster.compare('g9_owner_frontier', queries)
    statement = select_rows('g9_owner_frontier', 'alpha AND rareplanet')
    prefix = SETTINGS + 'SET enable_seqscan = off; SET enable_bitmapscan = on;\n'
    if hooks:
        cluster.run(prefix + 'SELECT pin.g2_inject(41, 1, false);\n' + statement,
                    error='Pin injected storage error', label='prove-owner-frontier')
        blocker = cluster.blocker('g9-owner-frontier-blocker')
        expected = json.loads(cluster.run(prefix + statement, label='owner-frontier-before-pause'))
        reader = cluster.start(prefix + 'SELECT pin.g2_inject(41, 1, true);\n' + statement,
                               'g9-owner-frontier-reader')
        cluster.wait_lock('g9-owner-frontier-reader', False)
        cluster.run("INSERT INTO g9_owner_frontier VALUES "
                    "(3000, 'alpha rareplanet concurrent', 0);",
                    label='writer-during-owner-frontier-read')
        cluster.release('g9-owner-frontier-blocker', blocker)
        if json.loads(cluster.finish(reader)) != expected:
            raise AssertionError('owner-frontier reader observed a later heap snapshot')
    cluster.run('VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_owner_frontier;',
                label='owner-frontier-vacuum')
    cluster.compare('g9_owner_frontier', queries)
    cluster.restart(immediate=True)
    cluster.compare('g9_owner_frontier', queries)
    cluster.run('DROP TABLE g9_owner_frontier;', label='owner-frontier-cleanup')

def anchor_seek_qualification(cluster: Cluster) -> None:
    query = 'alpha AND rareplanet'
    statement = select_rows('g9_anchor', query)
    prefix = (SETTINGS + 'SET pin.enable_owner_frontier = off;\n'
              + 'SET enable_seqscan = off; SET enable_bitmapscan = on;\n')
    expected = json.loads(cluster.run(prefix + statement, label='anchor-before-pause'))
    cluster.run(prefix + 'SELECT pin.g2_inject(40, 1, false);\n' + statement,
                error='Pin injected storage error', label='prove-frontier-seek')
    for override, source in (
        ('SET pin.enable_frontier_anchors = off;\n', statement),
        ('SET pin.enable_grouped_scan = off;\n', statement),
        ('', select_rows('g9_anchor', '"alpha beta"')),
    ):
        cluster.run(prefix + override + 'SELECT pin.g2_inject(40, 1, false);\n' + source,
                    label='prove-anchor-fallback')

    blocker = cluster.blocker('g9-anchor-blocker')
    reader = cluster.start(prefix + 'SELECT pin.g2_inject(40, 1, true);\n' + statement,
                           'g9-anchor-reader')
    cluster.wait_lock('g9-anchor-reader', False)
    cluster.run("SET statement_timeout = '10s'; INSERT INTO g9_anchor "
                "SELECT i, 'alpha rareplanet', 0 FROM generate_series(40000, 41023) AS i;",
                label='related-writer-during-anchor-read')
    maintenance = cluster.start(SETTINGS + 'SET pin.enable_grouped_storage = on;\n'
                                'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_anchor;',
                                'g9-anchor-maintenance')
    cluster.wait_structure_lock('g9-anchor-maintenance')
    cluster.release('g9-anchor-blocker', blocker)
    if json.loads(cluster.finish(reader)) != expected:
        raise AssertionError('anchored reader changed its heap-visible snapshot')
    cluster.finish(maintenance)
    cluster.compare('g9_anchor')
    # read-side hook errors and cancellation must not leave locks or broken cursors.
    cluster.run("INSERT INTO g9_anchor SELECT i, 'unrelated', 0 "
                'FROM generate_series(42000, 42999) AS i;')
    cluster.run("INSERT INTO g9_anchor SELECT i, 'alpha rareplanet', 0 "
                'FROM generate_series(43000, 44023) AS i;')
    blocker = cluster.blocker('g9-anchor-cancel-blocker')
    reader = cluster.start(prefix + 'SELECT pin.g2_inject(40, 1, true);\n' + statement,
                           'g9-anchor-cancel')
    cluster.wait_lock('g9-anchor-cancel', False)
    cluster.signal('g9-anchor-cancel', 'pg_cancel_backend')
    cluster.finish(reader, error='canceling statement due to user request')
    cluster.release('g9-anchor-cancel-blocker', blocker)
    cluster.compare('g9_anchor', (query,))
    cluster.run('SET pin.enable_frontier_anchors = off;\n'
                'SET pin.enable_grouped_storage = off;\n'
                'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_anchor;', label='anchor-invalidation')
    cluster.run(prefix + 'SELECT pin.g2_inject(40, 1, false);\n' + statement,
                label='prove-invalidated-fallback')
    cluster.restart(immediate=True)
    cluster.compare('g9_anchor')


def permissions(cluster: Cluster) -> None:
    cluster.run('CREATE ROLE pin_g9_reader;')
    for gate in ('storage', 'scan'):
        cluster.run(f'SET ROLE pin_g9_reader; SET pin.enable_grouped_{gate} = on;',
                    error='permission denied to set parameter', label='gate-permissions')
    cluster.run('SET ROLE pin_g9_reader; SET pin.enable_frontier_anchors = on;',
                error='permission denied to set parameter', label='anchor-permissions')
    cluster.run('SET ROLE pin_g9_reader; SET pin.enable_owner_frontier = on;',
                error='permission denied to set parameter', label='owner-frontier-permissions')
    anchor_default = cluster.run(
        "SELECT boot_val FROM pg_settings WHERE name = 'pin.enable_frontier_anchors'"
    )
    if anchor_default != 'off':
        raise AssertionError('frontier anchors must default off')
    owner_default = cluster.run(
        "SELECT boot_val FROM pg_settings WHERE name = 'pin.enable_owner_frontier'"
    )
    if owner_default != 'off':
        raise AssertionError('owner frontier must default off')
    cluster.run('DROP ROLE pin_g9_reader;')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--psql', required=True)
    parser.add_argument('--pg-ctl', required=True)
    parser.add_argument('--data', required=True, type=Path)
    parser.add_argument('--server-log', required=True, type=Path)
    parser.add_argument('--artifacts', required=True, type=Path)
    parser.add_argument('--hooks', choices=('0', '1'), required=True)
    parser.add_argument('--anchors', choices=('0', '1'), default='0')
    parser.add_argument('--owner-frontier', choices=('0', '1'), default='0')
    args = parser.parse_args()
    cluster = Cluster(args)
    results: dict[str, Any] = {'postgres': '18.6', 'hooks': args.hooks == '1',
                               'anchors': cluster.anchors,
                               'owner_frontier': cluster.owner_frontier, 'passed': False}
    try:
        cluster.run('CREATE EXTENSION pin;')
        if cluster.run('SHOW server_version_num') != '180006':
            raise AssertionError('wrong PostgreSQL version')
        available = cluster.run(
            "SELECT to_regprocedure('pin.g2_inject(integer,integer,boolean)') IS NOT NULL"
        ) == 't'
        if available != (args.hooks == '1'):
            raise AssertionError('normal and test-hook installation modes do not match')
        lifecycle = cluster.run('\\i ' + str(ROOT / 'tests/sql/g9_grouped.sql'), label='lifecycle')
        del lifecycle
        log = args.server_log.read_text(encoding='utf-8')
        if 'sort_spilled=true' not in log:
            raise AssertionError('no grouped sort spill was observed')
        if 'Pin grouped snapshot skipped: maintenance memory is too small' not in log:
            raise AssertionError('low-memory publication fallback was not observed')
        permissions(cluster)
        if cluster.anchors:
            cluster.run('\\i ' + str(ROOT / 'tests/sql/issue14_anchors.sql'), label='anchor-lifecycle')
        if available:
            prove_scan_selection(cluster)
            crash_boundaries(cluster)
            cancellation(cluster)
            concurrent_reader_writer_maintenance(cluster)
            if cluster.anchors:
                anchor_seek_qualification(cluster)
            if cluster.owner_frontier:
                owner_frontier_qualification(cluster, True)
        if cluster.owner_frontier and not available:
            owner_frontier_qualification(cluster, False)
        cluster.restart(immediate=False)
        cluster.compare('g9_small')
        stages = list((*STAGES, 39) if cluster.anchors else STAGES) if available else []
        if available and cluster.owner_frontier:
            stages.append(41)
        results.update(passed=True, stages=stages,
                       spill_observed=True, low_memory_fallback_observed=True)
    finally:
        cluster.cleanup()
        (args.artifacts / 'result.json').write_text(json.dumps(results, indent=2) + '\n')
    print(json.dumps(results, sort_keys=True))


if __name__ == '__main__':
    main()
