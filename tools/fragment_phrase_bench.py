#!/usr/bin/env python3
"""Warm serial phrase ablation; creates/drops a fixture in a disposable database."""
import argparse
import hashlib
import json
import platform
import subprocess
from pathlib import Path
import statistics
import time
import uuid

from g9_profile import Session
from g9_count_bench import cpu_read, cpu_delta


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--disposable', action='store_true', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--rows', type=int, default=256)
    parser.add_argument('--blocks', type=int, default=6)
    parser.add_argument('--queries', type=int, default=6)
    args = parser.parse_args()
    if min(args.rows, args.blocks, args.queries) < 2:
        parser.error('rows, blocks and queries must each be at least two')
    args.output.mkdir(parents=True, exist_ok=False)
    table = 'public.pin_fragment_' + uuid.uuid4().hex[:12]
    samples, checks, plans, statements = [], [], {}, []
    with Session(Path('/usr/lib/postgresql/18/bin/psql'), args.output / 'psql.stderr', 'pin_legacy') as s:
        def execute(sql):
            statements.append(sql)
            return s.execute(sql.rstrip(';') + ';')
        pid = int(execute('SELECT pg_backend_pid();'))
        revision = execute('SELECT pin.build_revision();')
        server = execute('SELECT version();')
        settings = execute('SHOW ALL;')
        (args.output / 'settings.txt').write_text(settings + '\n')
        execute(f'CREATE TABLE {table}(id int PRIMARY KEY, body text) WITH (autovacuum_enabled=false);')
        try:
            execute(f"INSERT INTO {table} SELECT i, repeat('echo ',10000) || CASE WHEN i%2=0 "
                    f"THEN 'alpha beta' ELSE 'beta alpha' END FROM generate_series(1,{args.rows}) i;")
            execute(f'CREATE INDEX ON {table} USING pin(body);')
            execute(f"CREATE INDEX ON {table} USING gin(to_tsvector('simple',body));")
            execute(f'VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0) {table};')
            execute('SET pin.enable_count_fastpath=off; SET pin.enable_grouped_count=off;')
            execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
            for name, source, gin in [('adjacent', '"alpha beta"', 'alpha <-> beta'),
                                      ('repeated', '"echo echo"', 'echo <-> echo'),
                                      ('nonadjacent', '"alpha echo"', 'alpha <-> echo')]:
                def query(mode, projection='count(*)'):
                    clause = (f"to_tsvector('simple',body) @@ to_tsquery('simple','{gin}')" if mode == 'gin'
                              else f"body OPERATOR(pin.@@@) pin.parse_query('{source}')")
                    return f'SELECT {projection} FROM ONLY {table} WHERE {clause}'
                def configure(mode):
                    execute('SET pin.enable_phrase_positions=' + ('on' if mode == 'positions' else 'off') + ';'
                            'SET enable_seqscan=' + ('on' if mode == 'oracle' else 'off') + ';'
                            'SET enable_bitmapscan=' + ('off' if mode == 'oracle' else 'on') + ';')
                identities = {}
                for mode in ('oracle', 'legacy', 'positions', 'gin'):
                    configure(mode)
                    identities[mode] = execute(query(mode, 'id,ctid') + ' ORDER BY id,ctid;')
                    plans[name + '-' + mode] = json.loads(execute(
                        'EXPLAIN (ANALYZE,BUFFERS,TIMING OFF,FORMAT JSON) ' + query(mode)))
                    plan_text = json.dumps(plans[name + '-' + mode])
                    expected = 'Seq Scan' if mode == 'oracle' else 'Bitmap Index Scan'
                    if expected not in plan_text:
                        raise AssertionError(f'{name}/{mode}: missing {expected}')
                if len(set(identities.values())) != 1:
                    raise AssertionError(identities)
                checks.append({'case': name, 'identities': identities,
                               'sha256': hashlib.sha256(identities['oracle'].encode()).hexdigest()})
                for block in range(args.blocks):
                    order = ('legacy', 'positions', 'gin') if block % 2 == 0 else ('gin', 'positions', 'legacy')
                    for mode in order:
                        configure(mode)
                        sql = query(mode)
                        for _ in range(2):
                            execute(sql)
                        before = cpu_read(pid)
                        latency, answers = [], []
                        for _ in range(args.queries):
                            start = time.perf_counter_ns()
                            answers.append(execute(sql))
                            latency.append((time.perf_counter_ns() - start) / 1e6)
                        after = cpu_read(pid)
                        if len(set(answers)) != 1:
                            raise AssertionError('unstable answers')
                        samples.append(dict(case=name, mode=mode, block=block, before=before, after=after,
                                            cpu=cpu_delta(before, after, args.queries), latency_ms=latency,
                                            answer=answers[0]))
                        (args.output / 'samples.json').write_text(json.dumps(samples, indent=2) + '\n')
            execute('ROLLBACK;')
        finally:
            execute('ROLLBACK;')
            execute(f'DROP TABLE {table};')
            (args.output / 'statements.sql').write_text('\n'.join(statements) + '\n')
    summary = []
    for name in ('adjacent', 'repeated', 'nonadjacent'):
        for mode in ('legacy', 'positions', 'gin'):
            selected = [x for x in samples if x['case'] == name and x['mode'] == mode]
            summary.append(dict(case=name, mode=mode, median_cpu_us=statistics.median(
                x['cpu']['cpu_us_per_query'] for x in selected)))
    (args.output / 'result.json').write_text(json.dumps(dict(
        revision=revision, server=server, platform=platform.platform(),
        cpu=subprocess.run(['lscpu'], capture_output=True, text=True, check=True).stdout, rows=args.rows, samples=samples, summary=summary, checks=checks), indent=2)+'\n')
    (args.output / 'plans.json').write_text(json.dumps(plans, indent=2)+'\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
