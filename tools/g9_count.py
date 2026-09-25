#!/usr/bin/env python3
"""Qualify grouped counts on a disposable G9 cluster; this restarts the server."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import time
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
QUERY = "SELECT count(*) FROM ONLY g9_count_docs WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha OR beta');"
COUNT = """
SET pin.enable_grouped_count = on;
SET pin.enable_count_fastpath = on;
SET pin.enable_count_vm = on;
SET pin.enable_grouped_scan = on;
SET pin.enable_grouped_storage = off;
SET enable_seqscan = off;
SET enable_bitmapscan = on;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SET max_parallel_workers_per_gather = 0;
SET lock_timeout = 0;
SET statement_timeout = '120s';
"""


def oracle(cluster: Any) -> int:
    # independent sequential row identities, not another count implementation.
    result = cluster.run("""
        SET pin.enable_count_fastpath = off; SET enable_seqscan = on;
        SET enable_bitmapscan = off; SET enable_indexscan = off; SET enable_indexonlyscan = off;
        SELECT coalesce(json_agg(id ORDER BY id), '[]'::json) FROM ONLY g9_count_docs
        WHERE body OPERATOR(pin.@@@) pin.parse_query('alpha OR beta');
    """, label='group-count-row-oracle')
    rows = json.loads(result)
    if len(rows) != len(set(rows)):
        raise AssertionError('row identity oracle contains duplicates')
    return len(rows)


def check(cluster: Any) -> None:
    expected = oracle(cluster)
    for vm in ('off', 'on'):
        settings = COUNT + f'SET pin.enable_count_vm = {vm};'
        actual = int(cluster.run(settings + QUERY, label='group-count-result'))
        plan = json.loads(cluster.run(settings + 'EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ' + QUERY,
                                      label='group-count-plan'))
        if actual != expected or plan[0]['Plan'].get('Custom Plan Provider') != 'PinCount':
            raise AssertionError(f'count/plan mismatch: {actual} != {expected}: {plan}')


def wait_retirement(cluster: Any, app: str) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        waiting = cluster.run(f"""SELECT EXISTS (
            SELECT FROM pg_stat_activity a JOIN pg_locks l USING (pid)
            WHERE a.application_name = '{app}' AND l.locktype = 'page'
              AND l.page = 2 AND l.mode = 'ExclusiveLock' AND NOT l.granted);""",
            label='group-retirement-lock')
        if waiting == 't':
            return
        time.sleep(0.05)
    raise AssertionError('VACUUM did not wait on grouped-count retirement protection')


def qualify(cluster: Any, hooks: bool) -> None:
    cluster.run('\\i ' + str(ROOT / 'tests/sql/g9_count.sql'), label='group-count-lifecycle')
    cluster.run('CREATE ROLE pin_group_count_reader;')
    try:
        cluster.run('SET ROLE pin_group_count_reader; SET pin.enable_grouped_count = on;',
                    error='permission denied to set parameter', label='group-count-privilege')
    finally:
        cluster.run('DROP ROLE pin_group_count_reader;')
    check(cluster)
    if hooks:
        for sequence, mode in enumerate(('finish', 'cancel', 'terminate'), 1):
            # leave a dead root so bulkdelete must run before any cleanup/rebuild.
            cluster.run(f'DELETE FROM g9_count_docs WHERE id = {100 + sequence};')
            expected = oracle(cluster)
            blocker_app = 'gc-block-' + mode
            reader_app = 'gc-read-' + mode
            vacuum_app = 'gc-vacuum-' + mode
            blocker = cluster.blocker(blocker_app)
            reader = cluster.start(COUNT + 'SELECT pin.g2_inject(14, 1, true);' + QUERY, reader_app)
            cluster.wait_lock(reader_app, False)
            vacuum = cluster.start("SET lock_timeout = 0; SET pin.enable_grouped_storage = off; "
                                   "VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_count_docs;", vacuum_app)
            wait_retirement(cluster, vacuum_app)
            # inserts must not wait behind the blocked VACUUM's writer lock.
            new_id = 20000 + sequence
            cluster.run(f"SET statement_timeout = '10s'; INSERT INTO g9_count_docs(id, body) "
                        f"VALUES ({new_id}, 'alpha beta concurrent');", label='group-count-writer')
            if mode == 'finish':
                cluster.release(blocker_app, blocker)
                result = int(cluster.finish(reader))
                if result != expected:
                    raise AssertionError(f'count observed a later snapshot: {result} != {expected}')
            else:
                function = 'pg_cancel_backend' if mode == 'cancel' else 'pg_terminate_backend'
                cluster.signal(reader_app, function)
                cluster.finish(reader, error='canceling statement' if mode == 'cancel' else 'terminating connection')
                # cleanup must release retirement while the pause blocker is still held.
                cluster.finish(vacuum)
                cluster.release(blocker_app, blocker)
            if mode == 'finish':
                cluster.finish(vacuum)
            check(cluster)
        # retain the old snapshot across a committed indexed update and VACUUM.
        expected = oracle(cluster)
        blocker = cluster.blocker('gc-snapshot-block')
        reader = cluster.start(COUNT + 'BEGIN ISOLATION LEVEL REPEATABLE READ;' + QUERY +
                               'SELECT pg_advisory_xact_lock(180006, 2);' + QUERY + 'COMMIT;',
                               'gc-snapshot-reader')
        cluster.wait_lock('gc-snapshot-reader', False)
        cluster.run("UPDATE g9_count_docs SET body = 'replacement' WHERE id BETWEEN 200 AND 240;")
        cluster.run('VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_count_docs;')
        cluster.release('gc-snapshot-block', blocker)
        counts = [int(line) for line in cluster.finish(reader).splitlines() if line.strip()]
        if counts != [expected, expected]:
            raise AssertionError(f'repeatable-read count changed: {counts}')
        check(cluster)
    cluster.restart(immediate=True)
    check(cluster)
    cluster.run('VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_count_docs;')
    check(cluster)


def main() -> None:
    if __package__:
        from .g9_qualification import Cluster
    else:
        from g9_qualification import Cluster
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
    result = {'passed': False, 'hooks': args.hooks == '1',
              'anchors': cluster.anchors, 'owner_frontier': cluster.owner_frontier}
    try:
        if cluster.run('SHOW server_version_num;') != '180006':
            raise AssertionError('grouped count qualification requires PostgreSQL 18.6')
        available = cluster.run(
            "SELECT to_regprocedure('pin.g2_inject(integer,integer,boolean)') IS NOT NULL;"
        ) == 't'
        if available != result['hooks']:
            raise AssertionError('normal and test-hook binaries do not match')
        result['revision'] = cluster.run('SELECT pin.build_revision();')
        qualify(cluster, available)
        result['passed'] = True
    finally:
        cluster.cleanup()
        (args.artifacts / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
