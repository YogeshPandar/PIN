#!/usr/bin/env python3
"""Crash/replay one mapped index in an explicitly selected disposable cluster."""
import argparse
import json
from pathlib import Path
import subprocess
import uuid

from g9_profile import Session


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--disposable', action='store_true', required=True)
    parser.add_argument('--cluster', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    pg = Path('/usr/lib/postgresql/18/bin')
    table = 'public.pin_replay_' + uuid.uuid4().hex[:12]
    events = []

    def execute(session, sql):
        result = session.execute(sql)
        events.append(dict(sql=sql, result=result))
        (args.output / 'events.json').write_text(json.dumps(events, indent=2) + '\n')
        return result

    def configure(session):
        execute(session, 'SET pin.enable_direct_documents=off; SET pin.enable_phrase_positions=on; '
                'SET enable_seqscan=off; SET enable_bitmapscan=on; SET enable_indexscan=off;')

    def check(session, expected):
        sql = f'''SELECT id,ctid FROM {table} WHERE body OPERATOR(pin.@@@)
                  pin.parse_query('"zulu omega"') ORDER BY id,ctid;'''
        result = execute(session, sql)
        assert [int(row.split('|')[0]) for row in result.splitlines()] == expected
        plan = execute(session, 'EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) ' + sql)
        assert 'Bitmap Index Scan' in plan
        execute(session, 'SET enable_seqscan=on; SET enable_bitmapscan=off;')
        assert execute(session, sql) == result
        configure(session)

    def control(*options):
        command = [str(pg / 'pg_ctl'), '-D', str(args.cluster.resolve()), '-t', '30', *options]
        result = subprocess.run(command, capture_output=True, text=True, timeout=40)
        events.append(dict(command=command, stdout=result.stdout, stderr=result.stderr,
                           returncode=result.returncode))
        (args.output / 'events.json').write_text(json.dumps(events, indent=2) + '\n')
        result.check_returncode()

    body = "repeat('middle ',60000)||'zulu omega'"
    with Session(pg / 'psql', args.output / 'before.stderr', 'pin_legacy') as session:
        actual = Path(execute(session, 'SHOW data_directory;')).resolve()
        if actual != args.cluster.resolve() or not actual.is_relative_to(Path('/tmp')):
            raise RuntimeError('refusing to stop a cluster other than the selected /tmp fixture')
        revision = execute(session, 'SELECT pin.build_revision();')
        for setting in ['fsync', 'full_page_writes', 'synchronous_commit']:
            assert execute(session, f'SHOW {setting};') == 'on'
        execute(session, f'CREATE TABLE {table}(id int PRIMARY KEY,body text) WITH (autovacuum_enabled=false);')
        execute(session, 'SET pin.enable_direct_documents=on;')
        execute(session, f'CREATE INDEX ON {table} USING pin(body);')
        configure(session)
        execute(session, 'CHECKPOINT;')
        execute(session, f'INSERT INTO {table} VALUES (1,{body});')
        execute(session, 'BEGIN;')
        execute(session, f'INSERT INTO {table} VALUES (2,{body});')
        check(session, [1, 2])
        control('stop', '-m', 'immediate')
    control('start', '-l', str(args.output.resolve() / 'recovery.log'))
    with Session(pg / 'psql', args.output / 'after.stderr', 'pin_legacy') as session:
        assert execute(session, 'SELECT pin.build_revision();') == revision
        configure(session)
        check(session, [1])
        execute(session, f'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) {table};')
        execute(session, f'INSERT INTO {table} VALUES (3,{body});')
        check(session, [1, 3])
        execute(session, 'SET pin.enable_direct_documents=on;')
        execute(session, f'REINDEX TABLE {table};')
        check(session, [1, 3])
        execute(session, f'DROP TABLE {table};')
    (args.output / 'result.json').write_text(json.dumps(dict(revision=revision,
        committed_survived=True, uncommitted_absent=True, vacuum_insert=True,
        reindex=True, cleaned=True), indent=2) + '\n')
    print('committed/uncommitted replay, VACUUM insertion and REINDEX comparisons passed')


if __name__ == '__main__':
    main()
