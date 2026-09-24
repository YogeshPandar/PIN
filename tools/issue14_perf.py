#!/usr/bin/env python3
"""Read-only paired legacy/grouped/GIN measurements on a prepared, quiet fixture.

Uses libpq environment variables and existing id/body plus Pin/GIN indexes.
Does not rebuild indexes, flush caches, disable durability, or start writers.
--local-proc asserts that PostgreSQL shares this process's Linux PID namespace.
See docs/issue-14-performance.md for preparation, profiling and acceptance gates.
"""
from __future__ import annotations

import argparse
from contextlib import AbstractContextManager
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import subprocess
import time
from typing import Any

from g6_latency import summarize

CASES = {
    'common': ('alpha', 'alpha'),
    'rare': ('rareplanet', 'rareplanet'),
    'and': ('alpha AND beta', 'alpha & beta'),
    'selective_and': ('alpha AND rareplanet', 'alpha & rareplanet'),
    'selective_and_reversed': ('rareplanet AND alpha', 'rareplanet & alpha'),
    'or': ('alpha OR rareplanet', 'alpha | rareplanet'),
    'not': ('alpha AND NOT beta', 'alpha & !beta'),
    'phrase': ('"beta gamma"', 'beta <-> gamma'),
    'prefix': ('rare*', 'rare:*'),
}
BASE_OPTIONS = (
    '-c enable_seqscan=off -c enable_bitmapscan=on -c enable_indexscan=off '
    '-c enable_indexonlyscan=off -c max_parallel_workers_per_gather=0 -c jit=off '
    '-c pin.enable_count_fastpath=off -c pin.enable_count_vm=off '
    '-c pin.enable_exact_bitmap=on -c pin.enable_grouped_storage=off '
    '-c default_transaction_read_only=on -c gin_fuzzy_search_limit=0 -c statement_timeout=120s'
)


def identifier(value: str) -> str:
    parts = value.split('.')
    if not 1 <= len(parts) <= 2 or any(re.fullmatch(r'[a-z_][a-z_0-9]*', p) is None for p in parts):
        raise ValueError(f'expected one or two ordinary lowercase identifiers: {value!r}')
    return '.'.join('"' + part + '"' for part in parts)


def literal(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def nodes(plan: dict[str, Any]):
    yield plan
    for child in plan.get('Plans', []):
        yield from nodes(child)


def plan_summary(plan: list[dict[str, Any]], expected_index: str) -> dict[str, Any]:
    root = plan[0]['Plan']
    all_nodes = list(nodes(root))
    indexes = [node for node in all_nodes if node.get('Node Type') == 'Bitmap Index Scan']
    if [node.get('Index Name') for node in indexes] != [expected_index.split('.')[-1]]:
        raise ValueError('requested bitmap index was not selected')
    if any(node.get('Node Type') in ('Gather', 'Gather Merge') for node in all_nodes):
        raise ValueError('parallel plan invalidates serial backend CPU attribution')
    # node buffers and elapsed times include descendants; never sum them over the tree.
    fields = ('Node Type', 'Index Name', 'Actual Rows', 'Actual Loops', 'Actual Total Time',
              'Shared Hit Blocks', 'Shared Read Blocks', 'Shared Dirtied Blocks',
              'Shared Written Blocks', 'Temp Read Blocks', 'Temp Written Blocks',
              'Exact Heap Blocks', 'Lossy Heap Blocks', 'Rows Removed by Index Recheck',
              'Rows Removed by Filter', 'WAL Records', 'WAL Bytes')
    return {
        'execution_ms': plan[0]['Execution Time'],
        'planning_ms': plan[0].get('Planning Time'),
        'root_buffers': {key: root.get(key, 0) for key in fields if 'Blocks' in key},
        'index_elapsed_ms': sum(node['Actual Total Time'] * node['Actual Loops'] for node in indexes),
        'nodes_inclusive': [{key: node[key] for key in fields if key in node} for node in all_nodes],
        'elapsed_is_not_cpu': True,
    }


def parse_proc_stat(text: str) -> dict[str, int]:
    head, separator, tail = text.rpartition(') ')
    if not separator or ' (' not in head:
        raise ValueError('invalid /proc stat framing')
    pid, command = head.split(' (', 1)
    fields = tail.split()
    if len(fields) < 22 or not command.startswith('postgres'):
        raise ValueError('stat is not a PostgreSQL backend')
    result = {'pid': int(pid), 'minor_faults': int(fields[7]), 'major_faults': int(fields[9]),
              'user_ticks': int(fields[11]), 'system_ticks': int(fields[12]),
              'start_ticks': int(fields[19]), 'rss_pages': int(fields[21])}
    if any(value < 0 for value in result.values()):
        raise ValueError('negative process counter')
    return result


def cpu_delta(before: dict[str, int], after: dict[str, int], hz: int, executions: int) -> dict[str, Any]:
    if hz <= 0 or executions <= 0:
        raise ValueError('invalid CPU sampling denominator')
    if any(before[key] != after[key] for key in ('pid', 'start_ticks')):
        raise ValueError('backend identity changed during sampling')
    deltas = {key: after[key] - before[key] for key in
              ('user_ticks', 'system_ticks', 'minor_faults', 'major_faults')}
    if any(value < 0 for value in deltas.values()):
        raise ValueError('backend counters moved backwards')
    ticks = deltas['user_ticks'] + deltas['system_ticks']
    return {**deltas, 'clock_ticks_per_second': hz, 'executions': executions,
            'backend_cpu_ms_per_query': 1000 * ticks / hz / executions,
            'one_tick_ms_per_query': 1000 / hz / executions,
            'cpu_resolution_qualified': ticks >= 100,
            'rss_pages_after': after['rss_pages'],
            'includes_executor_and_protocol_cpu': True}


def control_pair(before: float, after: float, tolerance: float) -> dict[str, Any]:
    if any(not math.isfinite(value) or value <= 0 for value in (before, after)):
        raise ValueError('invalid GIN control throughput')
    if not math.isfinite(tolerance) or not 0 <= tolerance <= 1:
        raise ValueError('invalid control tolerance')
    drift = max(before, after) / min(before, after) - 1
    return {'before_tps': before, 'after_tps': after, 'relative_drift': drift,
            'stable': drift <= tolerance, 'tolerance': tolerance}


def environment(engine: str) -> dict[str, str]:
    return dict(os.environ, LC_ALL='C', PGCONNECT_TIMEOUT='10', PGAPPNAME='pin-issue14',
                PGOPTIONS=(os.environ.get('PGOPTIONS', '') + ' ' + BASE_OPTIONS
                           + f' -c pin.enable_grouped_scan={"on" if engine == "grouped" else "off"}'))


class Session(AbstractContextManager):
    def __init__(self, psql: Path, env: dict[str, str], log: Path):
        self.log = log.open('w', encoding='utf-8')
        self.serial = 0
        try:
            self.process = subprocess.Popen(
                [str(psql), '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=self.log, text=True, env=env, bufsize=1)
        except BaseException:
            self.log.close()
            raise

    def query(self, statement: str) -> str:
        self.serial += 1
        marker = f'__pin_issue14_end_{self.serial}__'
        assert self.process.stdin is not None and self.process.stdout is not None
        self.process.stdin.write(statement.rstrip() + '\n\\echo ' + marker + '\n')
        self.process.stdin.flush()
        output = []
        for line in self.process.stdout:
            if line.strip() == marker:
                return ''.join(output).strip()
            output.append(line)
        raise RuntimeError('psql terminated; inspect the recorded session log')

    def __exit__(self, exc_type, exc_value, traceback):
        try:
            if self.process.stdin is not None:
                try:
                    self.process.stdin.close()
                except BrokenPipeError:
                    pass
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
            if self.process.stdout is not None:
                self.process.stdout.close()
        finally:
            self.log.close()
        return False


def probe_cpu(psql: Path, env: dict[str, str], query: str, count: int,
              executions: int, log: Path) -> dict[str, Any]:
    hz = os.sysconf('SC_CLK_TCK')
    with Session(psql, env, log) as session:
        identity = json.loads(session.query(
            "SELECT json_build_object('pid', pg_backend_pid(), 'started', "
            "extract(epoch from backend_start)) FROM pg_stat_activity WHERE pid = pg_backend_pid();"))
        pid = identity['pid']
        stat = Path(f'/proc/{pid}/stat')
        before = parse_proc_stat(stat.read_text())
        btime = next(int(line.split()[1]) for line in Path('/proc/stat').read_text().splitlines()
                     if line.startswith('btime '))
        if before['pid'] != pid or abs(btime + before['start_ticks'] / hz - identity['started']) > 5:
            raise RuntimeError('local PID does not identify the connected backend')
        session.query('PREPARE issue14_probe AS ' + query)
        if session.query('EXECUTE issue14_probe;') != str(count):
            raise RuntimeError('CPU probe row count disagrees with qualification')
        before = parse_proc_stat(stat.read_text())
        started = time.monotonic_ns()
        remaining = executions
        while remaining:
            # bounded duplex batches cannot deadlock on filled stdin/stdout pipes.
            batch = min(remaining, 32)
            output = session.query('EXECUTE issue14_probe;\n' * batch).splitlines()
            if output != [str(count)] * batch:
                raise RuntimeError('CPU probe returned different rows or failed statements')
            remaining -= batch
        elapsed = time.monotonic_ns() - started
        after = parse_proc_stat(stat.read_text())
        return {**cpu_delta(before, after, hz, executions), 'batch_wall_ms': elapsed / 1e6,
                'page_bytes': os.sysconf('SC_PAGE_SIZE'), 'backend_pid': pid,
                'hardware_counters': None, 'allocation_profile': None}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bindir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--table', default='public.pin_g6_bench')
    parser.add_argument('--pin-index', default='pin_g6_bench_body')
    parser.add_argument('--gin-index', default='pin_g6_bench_body_gin')
    parser.add_argument('--cases', nargs='+', choices=CASES, default=list(CASES))
    parser.add_argument('--samples', type=int, default=5)
    parser.add_argument('--seconds', type=int, default=10)
    parser.add_argument('--clients', type=int, default=1)
    parser.add_argument('--seed', type=int, default=14)
    parser.add_argument('--control-tolerance', type=float, default=0.10)
    parser.add_argument('--local-proc', action='store_true')
    parser.add_argument('--cpu-executions', type=int, default=1000)
    parser.add_argument('--label', default='warm-quiet-fixture')
    args = parser.parse_args()
    if not (1 <= args.samples <= 30 and 1 <= args.seconds <= 3600 and 1 <= args.clients <= 64
            and 1 <= args.cpu_executions <= 100_000):
        parser.error('samples: 1..30; seconds: 1..3600; clients: 1..64; CPU executions: 1..100000')
    if not math.isfinite(args.control_tolerance) or not 0 <= args.control_tolerance <= 1:
        parser.error('control tolerance must be finite and between zero and one')
    table = identifier(args.table)
    identifier(args.pin_index)
    identifier(args.gin_index)
    args.output.mkdir(parents=True, exist_ok=False)
    psql = args.bindir / 'psql'
    pgbench = args.bindir / 'pgbench'
    rng = random.Random(args.seed)

    def save(name: str, value: Any) -> None:
        path = args.output / name
        temporary = path.with_suffix(path.suffix + '.tmp')
        temporary.write_text(json.dumps(value, indent=2, allow_nan=False) + '\n', encoding='utf-8')
        temporary.replace(path)

    def sql(statement: str, engine: str) -> str:
        return subprocess.check_output([str(psql), '-X', '-qAt', '-v', 'ON_ERROR_STOP=1',
                                        '-c', statement], env=environment(engine), text=True).strip()

    settings = json.loads(sql("SELECT json_object_agg(name, setting) FROM pg_settings WHERE name IN "
        "('server_version','shared_buffers','work_mem','fsync','full_page_writes',"
        "'synchronous_commit','block_size','track_io_timing','effective_io_concurrency')", 'legacy'))
    save('environment.json', {'label': args.label, 'platform': platform.platform(),
        'cpu_count_client_host': os.cpu_count(), 'settings': settings,
        'server_build_revision': sql('SELECT pin.build_revision()', 'legacy'),
        'index_bytes': {engine: int(sql(f'SELECT pg_relation_size({literal(index)})', engine))
                        for engine, index in [('legacy', args.pin_index), ('gin', args.gin_index)]},
        'fixture': args.table, 'samples': args.samples, 'seconds': args.seconds,
        'clients': args.clients, 'seed': args.seed, 'cache_mode': 'warm-only',
        'local_proc_requested': args.local_proc, 'pgoptions': environment('grouped')['PGOPTIONS'],
        'limitations': ['same Pin physical index, grouped gate on/off; not separate index sizes',
                        'active grouped snapshot must be qualified separately',
                        'no cold-cache, writer, WAL-maintenance, ranking or TIN claim',
                        'transaction logging adds client overhead to throughput samples']})
    rows_by_case = {}
    scripts = {}
    for name in args.cases:
        pin, gin = CASES[name]
        predicates = {'pin': f'body OPERATOR(pin.@@@) pin.parse_query({literal(pin)})',
                      'gin': f"to_tsvector('simple', body) @@ to_tsquery('simple', {literal(gin)})"}
        row_queries = {engine: f'SELECT id FROM ONLY {table} WHERE {predicate}'
                       for engine, predicate in predicates.items()}
        difference = (f"SELECT count(*) FROM (({row_queries['pin']} EXCEPT ALL {row_queries['gin']}) "
                      f"UNION ALL ({row_queries['gin']} EXCEPT ALL {row_queries['pin']})) AS d")
        for engine in ('legacy', 'grouped'):
            if sql(difference, engine) != '0':
                raise RuntimeError(f'{name}/{engine}: row identities differ from GIN')
        count = int(sql(f'SELECT count(*) FROM ({row_queries["gin"]}) AS d', 'gin'))
        rows_by_case[name] = count
        for engine in ('legacy', 'grouped', 'gin'):
            predicate = predicates['gin' if engine == 'gin' else 'pin']
            statement = f'SELECT count(*) FROM ONLY {table} WHERE {predicate};'
            path = args.output / f'{name}-{engine}.sql'
            path.write_text(statement + '\n', encoding='utf-8')
            scripts[name, engine] = (path, statement)
    save('identity-qualification.json', {'symmetric_difference': 0, 'matching_rows': rows_by_case,
                                       'scope': 'one statement snapshot per engine comparison'})

    def run(name: str, engine: str, sample: int, suffix: str = '') -> dict[str, Any]:
        key = f'{name}-{sample}-{engine}{suffix}'
        script, statement = scripts[name, engine]
        env = environment(engine)
        command = [str(pgbench), '-n', '-M', 'prepared', '-c', str(args.clients),
                   '-j', str(min(args.clients, 4)), '-f', str(script)]
        subprocess.run(command + ['-T', '1'], env=env, check=True, capture_output=True)
        plan = json.loads(sql('EXPLAIN (ANALYZE, BUFFERS, WAL, FORMAT JSON) ' + statement, engine))
        summary = plan_summary(plan, args.gin_index if engine == 'gin' else args.pin_index)
        save(key + '-plan.json', plan)
        prefix = (args.output / (key + '.latency')).resolve()
        started = time.monotonic_ns()
        result = subprocess.run(command + ['-T', str(args.seconds), '-l', '--log-prefix', str(prefix)],
                                env=env, check=True, capture_output=True, text=True)
        elapsed = time.monotonic_ns() - started
        (args.output / (key + '.txt')).write_text(result.stdout + result.stderr, encoding='utf-8')
        tps = re.search(r'^tps = ([0-9.]+)', result.stdout, re.MULTILINE)
        failures = re.search(r'number of failed transactions: (\d+)', result.stdout)
        latency = summarize(args.output.glob(prefix.name + '.*'))
        if (tps is None or failures is None or int(failures[1]) != 0
                or not latency['service_latency']['count'] or any(latency['failures'].values())):
            raise RuntimeError(f'{key}: missing samples or failed transactions')
        measurement = {'case': name, 'engine': engine, 'sample': sample, 'suffix': suffix,
                       'tps': float(tps[1]), 'latency': latency, 'plan': summary,
                       'runner_wall_ms': elapsed / 1e6}
        save(key + '-summary.json', measurement)
        return measurement

    pairs = []
    for sample in range(args.samples):
        order = list(args.cases)
        rng.shuffle(order)
        for name in order:
            before = run(name, 'gin', sample, '-before')
            pin_order = ['legacy', 'grouped']
            if sample % 2:
                pin_order.reverse()
            pins = {engine: run(name, engine, sample) for engine in pin_order}
            after = run(name, 'gin', sample, '-after')
            control = control_pair(before['tps'], after['tps'], args.control_tolerance)
            baseline = (before['tps'] + after['tps']) / 2
            pair = {'case': name, 'sample': sample, 'order': ['gin-before', *pin_order, 'gin-after'],
                    'control': control, 'pin': pins,
                    'ratios_to_paired_gin': {engine: value['tps'] / baseline for engine, value in pins.items()},
                    'eligible_for_comparison': control['stable']}
            pairs.append(pair)
            save('pairs.json', pairs)
            print(f'{name} sample={sample} stable_control={control["stable"]}', flush=True)
    if args.local_proc:
        cpu = []
        for sample in range(args.samples):
            jobs = [(name, engine) for name in args.cases for engine in ('legacy', 'grouped', 'gin')]
            rng.shuffle(jobs)
            for name, engine in jobs:
                _, statement = scripts[name, engine]
                result = probe_cpu(psql, environment(engine), statement, rows_by_case[name],
                                   args.cpu_executions, args.output / f'{name}-{sample}-{engine}-cpu.log')
                cpu.append({'case': name, 'engine': engine, 'sample': sample, **result})
                save('backend-cpu.json', cpu)
    save('status.json', {'completed': True, 'all_controls_stable': all(p['control']['stable'] for p in pairs),
                         'performance_target_qualified': False,
                         'reason': 'hardware profiles, workload breadth and native correctness gates remain'})


if __name__ == '__main__':
    main()
