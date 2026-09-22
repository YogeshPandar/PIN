#!/usr/bin/env python3
"""Read-only, serial bitmap comparison on the existing G6 benchmark fixture.

Requires public.pin_g6_bench plus Pin and simple-config expression GIN indexes.
Uses libpq environment variables. No schema, data or server settings are changed.
Only the fixed queries below are compared; analyzer equivalence is not implied.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import re
import subprocess

from g6_latency import summarize

CASES = {
    'common': ('alpha', 'alpha'),
    'rare': ('rareplanet', 'rareplanet'),
    'and': ('alpha AND beta', 'alpha & beta'),
    'or': ('alpha OR rareplanet', 'alpha | rareplanet'),
    'phrase': ('"beta gamma"', 'beta <-> gamma'),
}
OPTIONS = ('-c enable_seqscan=off -c enable_bitmapscan=on '
           '-c enable_indexscan=off -c enable_indexonlyscan=off '
           '-c max_parallel_workers_per_gather=0 -c jit=off '
           '-c pin.enable_count_fastpath=off -c pin.enable_count_vm=off '
           '-c statement_timeout=120s')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bindir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=3)
    parser.add_argument('--seconds', type=int, default=5)
    parser.add_argument('--exact-bitmap', choices=('on', 'off'), default='on')
    args = parser.parse_args()
    if not 1 <= args.samples <= 20 or not 1 <= args.seconds <= 120:
        parser.error('samples must be 1..20 and seconds 1..120')
    args.output.mkdir(parents=True, exist_ok=False)
    options = OPTIONS + f' -c pin.enable_exact_bitmap={args.exact_bitmap}'
    env = dict(os.environ, PGOPTIONS=options)
    psql = [str(args.bindir / 'psql'), '-X', '-qAt', '-v', 'ON_ERROR_STOP=1']

    def sql(statement: str) -> str:
        return subprocess.check_output(psql + ['-c', statement], env=env, text=True).strip()

    def save(name: str, value: object) -> None:
        (args.output / name).write_text(json.dumps(value, indent=2) + '\n')

    settings = json.loads(sql("SELECT json_object_agg(name, setting) FROM pg_settings "
                             "WHERE name IN ('server_version','shared_buffers','work_mem',"
                             "'fsync','full_page_writes','synchronous_commit','block_size')"))
    save('environment.json', {'platform': platform.platform(), 'cpu_count': os.cpu_count(),
                             'server_build_revision': sql('SELECT pin.build_revision()'),
                             'pgoptions': options, 'settings': settings,
                             'samples': args.samples, 'seconds': args.seconds,
                             'clients': 4, 'threads': 2,
                             'revision': subprocess.check_output(
                                 ['git', 'rev-parse', 'HEAD'], text=True).strip(),
                             'dirty': bool(subprocess.check_output(
                                 ['git', 'status', '--porcelain'], text=True))})
    queries = {}
    for name, (pin, gin) in CASES.items():
        predicates = {'pin': f"body OPERATOR(pin.@@@) pin.parse_query('{pin}')",
                      'gin': f"to_tsvector('simple', body) @@ to_tsquery('simple', '{gin}')"}
        rows = {engine: f'SELECT id FROM ONLY public.pin_g6_bench WHERE {predicate}'
                for engine, predicate in predicates.items()}
        difference = sql(f"SELECT count(*) FROM (({rows['pin']} EXCEPT ALL {rows['gin']}) "
                         f"UNION ALL ({rows['gin']} EXCEPT ALL {rows['pin']})) AS d")
        if difference != '0':
            raise RuntimeError(f'{name}: engines disagree on row identities')
        count = int(sql(f'SELECT count(*) FROM ({rows["pin"]}) AS d'))
        if count == 0:
            raise RuntimeError(f'{name}: fixture has no matches')
        save(f'{name}-correctness.json', {'matching_rows': count, 'symmetric_difference': 0})
        for engine, predicate in predicates.items():
            key = f'{name}-{engine}'
            query = f'SELECT count(*) FROM ONLY public.pin_g6_bench WHERE {predicate};'
            plan = json.loads(sql('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ' + query))
            expected_index = ('pin_g6_bench_body' if engine == 'pin'
                              else 'pin_g6_bench_body_gin')
            stack = [plan[0]['Plan']]
            index_names = []
            while stack:
                node = stack.pop()
                if node.get('Node Type') == 'Bitmap Index Scan':
                    index_names.append(node.get('Index Name'))
                stack.extend(node.get('Plans', []))
            if index_names != [expected_index]:
                raise RuntimeError(f'{key}: bitmap index plan was not selected')
            save(f'{key}-plan.json', plan)
            script = args.output / f'{key}.sql'
            script.write_text(query + '\n')
            queries[key] = script
    results = []
    for sample in range(args.samples):
        # alternate execution order to reduce systematic warmup/order bias.
        order = list(queries)
        if sample % 2:
            order.reverse()
        for key in order:
            base = [str(args.bindir / 'pgbench'), '-n', '-M', 'prepared', '-c', '4',
                    '-j', '2', '-f', str(queries[key])]
            subprocess.run(base + ['-T', '1'], env=env, check=True, capture_output=True)
            prefix = args.output / f'{key}-{sample}.latency'
            result = subprocess.run(base + ['-T', str(args.seconds), '-l',
                                            '--log-prefix', str(prefix)],
                                    env=env, check=True, capture_output=True, text=True)
            (args.output / f'{key}-{sample}.txt').write_text(result.stdout + result.stderr)
            latency = summarize(args.output.glob(prefix.name + '.*'))
            tps = re.search(r'^tps = ([\d.]+)', result.stdout, re.MULTILINE)
            failures = re.search(r'number of failed transactions: (\d+)', result.stdout)
            if tps is None or failures is None or int(failures[1]) != 0:
                raise RuntimeError(f'{key}: missing metrics or failed transactions')
            results.append({'case': key, 'sample': sample, 'tps': float(tps[1]),
                            'latency': latency})
            save('results.json', results)
            print(f'{key} sample={sample} tps={tps[1]}', flush=True)


if __name__ == '__main__':
    main()
