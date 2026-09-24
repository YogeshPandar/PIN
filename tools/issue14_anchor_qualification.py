#!/usr/bin/env python3
"""Exercise anchored frontiers in the G9 disposable PostgreSQL 18.6 cluster."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

import g9_qualification as g9

ROOT = Path(__file__).resolve().parents[1]
ANCHORS = 'SET pin.enable_frontier_anchors = on;\n'
STAGES = (39, 9, 10, 11, *g9.STAGES)
SCAN = g9.SETTINGS + '''
SET work_mem = '64MB';
SET enable_seqscan = off;
SET enable_bitmapscan = on;
'''


class Cluster(g9.Cluster):
    def run(self, sql: str, **kwargs: Any) -> str:
        return super().run(ANCHORS + sql, **kwargs)

    def start(self, sql: str, app: str):
        return super().start(ANCHORS + sql, app)

    def fixture(self) -> None:
        self.run(g9.SETTINGS + '''
            SET maintenance_work_mem = '16MB';
            SET pin.enable_grouped_storage = on;
            DROP TABLE IF EXISTS g9_crash;
            CREATE TABLE g9_crash(id integer, body text)
                WITH (autovacuum_enabled = false);
            INSERT INTO g9_crash VALUES
                (1, 'alpha'), (2, 'beta'), (3, 'alpha beta'), (4, 'alpha beta delta');
            INSERT INTO g9_crash SELECT i, 'alpha beta'
                FROM generate_series(100, 4195) AS i;
            CREATE INDEX g9_crash_pin ON g9_crash USING pin(body);
            SET pin.enable_grouped_storage = off;
            INSERT INTO g9_crash SELECT i, 'unrelated filler'
                FROM generate_series(4196, 5219) AS i;
            INSERT INTO g9_crash SELECT i, 'alpha beta delta'
                FROM generate_series(5220, 6243) AS i;
            DELETE FROM g9_crash WHERE id = 1;
            CHECKPOINT;
        ''', label='anchored-history-fixture')


def prove_seek(cluster: Cluster) -> None:
    cluster.fixture()
    statement = g9.select_rows('g9_crash', 'alpha AND beta')
    hook = 'SELECT pin.g2_inject(40, 1, false);\n'
    cluster.run(SCAN + hook + statement, error='Pin injected storage error', label='anchor-seek-used')
    expected = json.loads(cluster.run(SCAN + statement))
    actual = json.loads(cluster.run(SCAN + 'SET pin.enable_frontier_anchors = off;\n'
                                    + hook + statement, label='anchor-read-disabled'))
    if actual != expected:
        raise AssertionError('disabling anchor reads changed row identities')
    cluster.run('SET pin.enable_frontier_anchors = off; SET pin.enable_grouped_storage = off;\n'
                'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_crash;', label='invalidate-with-gates-off')
    actual = json.loads(cluster.run(SCAN + hook + statement, label='invalidated-anchor-fallback'))
    if actual != expected:
        raise AssertionError('canonical fallback changed row identities')


def seek_concurrency(cluster: Cluster) -> None:
    statement = g9.select_rows('g9_crash', 'alpha AND beta')
    for cancel in (False, True):
        cluster.fixture()
        expected = json.loads(cluster.run(SCAN + statement))
        blocker = cluster.blocker('anchor-seek-blocker')
        reader = cluster.start(SCAN + 'SELECT pin.g2_inject(40, 1, true);\n'
                               + statement, 'anchor-reader')
        cluster.wait_lock('anchor-reader', False)
        cluster.run("SET statement_timeout = '10s'; INSERT INTO g9_crash VALUES (5, 'alpha beta');",
                    label='related-append-during-seek')
        maintenance = cluster.start(cluster.vacuum(), 'anchor-maintenance')
        cluster.wait_structure_lock('anchor-maintenance')
        if cancel:
            cluster.signal('anchor-reader', 'pg_cancel_backend')
            cluster.finish(reader, error='canceling statement due to user request')
        cluster.release('anchor-seek-blocker', blocker)
        if not cancel and json.loads(cluster.finish(reader)) != expected:
            raise AssertionError('anchored reader exposed a later committed heap version')
        cluster.finish(maintenance)
        cluster.compare('g9_crash')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--psql', required=True)
    parser.add_argument('--pg-ctl', required=True)
    parser.add_argument('--data', required=True, type=Path)
    parser.add_argument('--server-log', required=True, type=Path)
    parser.add_argument('--artifacts', required=True, type=Path)
    parser.add_argument('--hooks', choices=('0', '1'), required=True)
    args = parser.parse_args()
    # only the wrapper's new local cluster is eligible for deliberate crash tests.
    data = args.data.absolute()
    if (data.name != 'data' or data.parent.parent != Path('/tmp')
            or not data.parent.name.startswith('pin-g9.') or data.is_symlink()
            or data.parent.is_symlink() or not (data / 'postmaster.pid').is_file()):
        parser.error('requires the disposable /tmp/pin-g9.*/data cluster')
    cluster = Cluster(args)
    result: dict[str, Any] = {'passed': False, 'hooks': args.hooks == '1', 'stages': []}
    try:
        if cluster.run('SHOW server_version_num') != '180006':
            raise AssertionError('requires PostgreSQL 18.6')
        if cluster.run('SELECT current_setting(\'data_directory\')') != str(data):
            raise AssertionError('connection does not address the disposable data directory')
        if cluster.run("SELECT bool_and(setting = 'on') FROM pg_settings WHERE name IN "
                       "('fsync', 'full_page_writes', 'synchronous_commit')") != 't':
            raise AssertionError('durability settings must be enabled')
        cluster.run('CREATE EXTENSION pin;')
        default = cluster.run("SELECT boot_val || ':' || context FROM pg_settings "
                              "WHERE name = 'pin.enable_frontier_anchors'")
        if default != 'off:superuser':
            raise AssertionError('anchor gate is not default-off and privileged')
        available = cluster.run(
            "SELECT to_regprocedure('pin.g2_inject(integer,integer,boolean)') IS NOT NULL"
        ) == 't'
        if available != result['hooks']:
            raise AssertionError('test-hook installation mode does not match')
        cluster.run('CREATE ROLE pin_anchor_reader;')
        cluster.run('SET ROLE pin_anchor_reader; SET pin.enable_frontier_anchors = on;',
                    error='permission denied to set parameter', label='anchor-permissions')
        cluster.run('DROP ROLE pin_anchor_reader;')
        cluster.run('\\i ' + str(ROOT / 'tests/sql/issue14_anchors.sql'), label='anchor-lifecycle')
        if available:
            prove_seek(cluster)
            seek_concurrency(cluster)
            g9.crash_boundaries(cluster, stages=STAGES)
            g9.concurrent_reader_writer_maintenance(cluster)
            result['stages'] = list(STAGES) + [40]
        cluster.fixture()
        cluster.restart(immediate=False)
        cluster.compare('g9_crash')
        result['passed'] = True
    except BaseException as error:
        result['error'] = repr(error)
        raise
    finally:
        cluster.cleanup()
        (args.artifacts / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        checksums = {str(path.relative_to(args.artifacts)): hashlib.sha256(path.read_bytes()).hexdigest()
                     for path in sorted(args.artifacts.rglob('*')) if path.is_file()
                     and path.name != 'sha256.json'}
        (args.artifacts / 'sha256.json').write_text(json.dumps(checksums, indent=2) + '\n')
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
