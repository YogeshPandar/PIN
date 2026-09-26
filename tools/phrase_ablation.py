#!/usr/bin/env python3
"""Paired ordinary PostgreSQL bitmap phrase scans on an existing G9 fixture."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import time

from g9_count_bench import TABLE, cpu_delta, cpu_read, predicate
from g9_profile import Session


def configure(session: Session, mode: str) -> None:
    session.execute('SET pin.enable_count_fastpath=off; SET pin.enable_grouped_count=off; '
                    'SET pin.enable_grouped_scan=on; SET pin.enable_exact_bitmap=on; '
                    'SET enable_seqscan=off; SET enable_bitmapscan=on; '
                    'SET enable_indexscan=off; SET jit=off; '
                    'SET pin.enable_phrase_positions=' + ('on' if mode == 'positions' else 'off') + ';')


def sql(mode: str, source: str, gin: str) -> str:
    if mode == 'gin':
        where = f"to_tsvector('simple',body) @@ to_tsquery('simple','{gin}')"
    else:
        where = f"body OPERATOR(pin.@@@) pin.parse_query('{source}')"
    return f'SELECT count(*) FROM ONLY {TABLE} WHERE {where};'


def identities(session: Session, source: str, gin: str) -> dict:
    result = {}
    for mode in ('legacy', 'positions', 'gin'):
        configure(session, mode)
        statement = sql(mode, source, gin).replace('SELECT count(*)', 'SELECT id,ctid').removesuffix(';')
        rows = session.execute(statement + ' ORDER BY id,ctid;')
        result[mode] = {'sha256': hashlib.sha256(rows.encode()).hexdigest(),
                        'rows': len(rows.splitlines()) if rows else 0}
    if len({tuple(item.values()) for item in result.values()}) != 1:
        raise AssertionError(f'full phrase identities differ: {source}: {result}')
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--queries', type=int, default=12)
    parser.add_argument('--blocks', type=int, default=6)
    args = parser.parse_args()
    if args.queries < 2 or args.blocks < 2:
        parser.error('at least two queries and blocks required')
    args.output.mkdir(parents=True, exist_ok=False)
    corpus = [('bravo_charlie', '"bravo charlie"', 'bravo <-> charlie'),
              ('delta_echo', '"delta echo"', 'delta <-> echo'),
              ('reversed', '"charlie bravo"', 'charlie <-> bravo')]
    records = []
    with Session(Path('/usr/lib/postgresql/18/bin/psql'), args.output / 'psql.stderr',
                 'pin_legacy') as session:
        pid = int(session.execute('SELECT pg_backend_pid();'))
        revision = session.execute('SELECT pin.build_revision();')
        session.execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
        for name, source, gin in corpus:
            identities(session, source, gin)
            plans = {}
            for mode in ('legacy', 'positions', 'gin'):
                configure(session, mode)
                plan = json.loads(session.execute('EXPLAIN (ANALYZE,BUFFERS,TIMING OFF,FORMAT JSON) '
                                                  + sql(mode, source, gin)))
                plans[mode] = plan
            (args.output / f'{name}-plans.json').write_text(json.dumps(plans, indent=2) + '\n')
            for block in range(args.blocks):
                order = ('legacy', 'positions', 'gin') if block % 2 == 0 else ('gin', 'positions', 'legacy')
                for mode in order:
                    configure(session, mode)
                    statement = sql(mode, source, gin)
                    for _ in range(2):
                        session.execute(statement)
                    before = cpu_read(pid)
                    latency = []
                    answers = []
                    for _ in range(args.queries):
                        start = time.perf_counter_ns()
                        answers.append(session.execute(statement))
                        latency.append((time.perf_counter_ns() - start) / 1e6)
                    after = cpu_read(pid)
                    if len(set(answers)) != 1:
                        raise AssertionError('result changed in read-only snapshot')
                    records.append({'case': name, 'block': block, 'mode': mode, 'answer': answers[0],
                                    'cpu': cpu_delta(before, after, args.queries),
                                    'latency_ms': latency})
        session.execute('ROLLBACK;')
    summary = []
    for name, _, _ in corpus:
        for mode in ('legacy', 'positions', 'gin'):
            selected = [r for r in records if r['case'] == name and r['mode'] == mode]
            summary.append({'case': name, 'mode': mode,
                            'median_cpu_us': statistics.median(r['cpu']['cpu_us_per_query'] for r in selected),
                            'median_latency_ms': statistics.median(v for r in selected for v in r['latency_ms']),
                            'answer': selected[0]['answer']})
    data = {'revision': revision, 'postgres': '18.6', 'db': os.environ.get('PGDATABASE'),
            'identities': 'full id,ctid stream compared for all three modes',
            'samples': records, 'summary': summary}
    (args.output / 'result.json').write_text(json.dumps(data, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
