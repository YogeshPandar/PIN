#!/usr/bin/env python3
"""Read-only paired G9-enabled/legacy/GIN profiling on the existing G6 fixture.

Uses persistent psql sessions and one exported snapshot. Run on a disposable,
quiescent database: the retained snapshot delays vacuum. This is a warm, serial
count workload, not a cold-cache, concurrent-write or ranked retrieval benchmark.
"""
from __future__ import annotations

import argparse
from contextlib import ExitStack
from dataclasses import dataclass
import itertools
import json
import math
import os
from pathlib import Path
import platform
import re
import select
import subprocess
import time
import uuid

CASES = {
    'common': ('alpha', 'alpha'),
    'rare': ('rareplanet', 'rareplanet'),
    'and': ('alpha AND beta', 'alpha & beta'),
    'selective_and': ('alpha AND rareplanet', 'alpha & rareplanet'),
    'or': ('alpha OR rareplanet', 'alpha | rareplanet'),
    'not': ('alpha AND NOT rareplanet', 'alpha & !rareplanet'),
    'phrase': ('"beta gamma"', 'beta <-> gamma'),
    'prefix': ('alp*', 'alp:*'),
    'absent': ('missingplanet', 'missingplanet'),
}
MODES = ('pin_grouped_enabled', 'pin_legacy', 'gin')
TABLE = 'ONLY public.pin_g6_bench'
INDEX = {'pin_grouped_enabled': 'pin_g6_bench_body',
         'pin_legacy': 'pin_g6_bench_body', 'gin': 'pin_g6_bench_body_gin'}
if __package__:
    from . import frontier_options, owner_frontier_options
else:
    import frontier_options
    import owner_frontier_options

SETTINGS = {
    'enable_seqscan': 'off', 'enable_bitmapscan': 'on',
    'enable_indexscan': 'off', 'enable_indexonlyscan': 'off',
    'max_parallel_workers_per_gather': '0', 'jit': 'off',
    'pin.enable_count_fastpath': 'off', 'pin.enable_count_vm': 'off',
    'pin.enable_exact_bitmap': 'on', 'pin.enable_grouped_storage': 'off',
    'statement_timeout': '120s', 'lock_timeout': '10s', 'work_mem': '64MB',
    'plan_cache_mode': 'force_generic_plan', 'standard_conforming_strings': 'on',
}


def literal(text: str) -> str:
    if '\x00' in text or '\\' in text or '\n' in text or '\r' in text:
        raise ValueError('unsupported SQL literal')
    return "'" + text.replace("'", "''") + "'"


class Session:
    """One bounded, fail-closed psql pipe; no connection startup in samples."""

    def __init__(self, psql: Path, log: Path, mode: str, timeout: float = 125,
                 *, frontier_anchors: bool = False, owner_frontier: bool = False):
        self.timeout = timeout
        self.pending = bytearray()
        self.log = log.open('xb')
        options = frontier_options.options(SETTINGS, frontier_anchors)
        options = owner_frontier_options.options(options, owner_frontier)
        options['pin.enable_grouped_scan'] = 'on' if mode == MODES[0] else 'off'
        if mode == 'oracle':
            options.update(enable_seqscan='on', enable_bitmapscan='off')
        self.options = options
        env = dict(os.environ, PGAPPNAME='pin_g9_profile_' + mode,
                   PGCONNECT_TIMEOUT='10', LC_ALL='C',
                   PGOPTIONS=' '.join(f'-c {key}={value}' for key, value in options.items()))
        try:
            self.process = subprocess.Popen(
                [str(psql), '-X', '-qAtw', '-v', 'ON_ERROR_STOP=1', '-P', 'pager=off'],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
                env=env, bufsize=0,
            )
        except BaseException:
            self.log.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self) -> None:
        # closing a connection rolls back the read-only transaction, including on error.
        if self.process.stdin:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        if self.process.stdout:
            self.process.stdout.close()
        self.log.close()

    def execute(self, sql: str) -> str:
        if not sql.rstrip().endswith(';') or '\x00' in sql:
            raise ValueError('complete SQL statement required')
        marker = ('pin_end_' + uuid.uuid4().hex).encode('ascii')
        payload = sql.encode('utf-8') + b'\n\\echo ' + marker + b'\n'
        if len(payload) > 4096:
            raise ValueError('command exceeds bounded pipe request')
        if self.process.poll() is not None:
            raise RuntimeError('psql exited; inspect session stderr')
        assert self.process.stdin is not None and self.process.stdout is not None
        written = self.process.stdin.write(payload)
        if written != len(payload):
            raise RuntimeError('short psql pipe write')
        self.process.stdin.flush()
        deadline = time.monotonic() + self.timeout
        output = bytearray()
        while True:
            while b'\n' in self.pending:
                line, _, rest = self.pending.partition(b'\n')
                self.pending = bytearray(rest)
                if line.rstrip(b'\r') == marker:
                    return output.decode('utf-8').strip()
                output.extend(line + b'\n')
                if len(output) > 4 * 1024 * 1024:
                    raise RuntimeError('response exceeds 4 MiB bound')
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.process.stdout], [], [], remaining)[0]:
                raise TimeoutError('psql response deadline exceeded')
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError('psql failed before response marker; inspect session stderr')
            self.pending.extend(chunk)
            if len(self.pending) > 4 * 1024 * 1024:
                raise RuntimeError('response line exceeds 4 MiB bound')


def predicate(case: str, mode: str) -> str:
    pin, gin = CASES[case]
    if mode == 'gin':
        return f"to_tsvector('simple', body) @@ to_tsquery('simple', {literal(gin)})"
    return f'body OPERATOR(pin.@@@) pin.parse_query({literal(pin)})'


def inspect_plan(plan: list, mode: str) -> dict:
    if not isinstance(plan, list) or len(plan) != 1 or not isinstance(plan[0], dict):
        raise ValueError('one JSON plan is required')
    stack = [plan[0]['Plan']]
    nodes = []
    while stack:
        node = stack.pop()
        nodes.append(node)
        stack.extend(node.get('Plans', []))
    if mode == 'oracle':
        expected = ('Aggregate', 'Seq Scan')
        if (any(node.get('Node Type') not in expected for node in nodes)
                or sum(node.get('Node Type') == 'Seq Scan' for node in nodes) != 1):
            raise ValueError('heap oracle must use one sequential scan without custom nodes')
        return {'heap_oracle': True}
    allowed = ('Aggregate', 'Bitmap Heap Scan', 'Bitmap Index Scan')
    indexes = [node for node in nodes if node.get('Node Type') == 'Bitmap Index Scan']
    heaps = [node for node in nodes if node.get('Node Type') == 'Bitmap Heap Scan']
    if (any(node.get('Node Type') not in allowed for node in nodes)
            or len(indexes) != 1 or indexes[0].get('Index Name') != INDEX[mode]
            or len(heaps) != 1 or heaps[0].get('Relation Name') != 'pin_g6_bench'):
        raise ValueError(f'{mode}: expected one serial bitmap index and heap scan')
    # parent counters include children; keep each scope separate rather than summing.
    keys = ('Actual Rows', 'Actual Loops', 'Actual Total Time', 'Shared Hit Blocks',
            'Shared Read Blocks', 'Temp Read Blocks', 'Temp Written Blocks',
            'Exact Heap Blocks', 'Lossy Heap Blocks', 'Rows Removed by Index Recheck')
    return {'execution_ms': plan[0].get('Execution Time'),
            'index': {key: indexes[0][key] for key in keys if key in indexes[0]},
            'bitmap_heap_inclusive': {key: heaps[0][key] for key in keys if key in heaps[0]},
            'root_inclusive': {key: plan[0]['Plan'][key] for key in keys if key in plan[0]['Plan']}}


def matching_rows(sessions: dict[str, Session], case: str) -> int:
    # compare full ordered physical identities, not equal counts or collision-prone hashes.
    for mode, session in sessions.items():
        session.execute(f'DECLARE identities NO SCROLL CURSOR FOR SELECT ctid FROM {TABLE} '
                        f'WHERE {predicate(case, mode)} ORDER BY ctid;')
    count = 0
    while True:
        batches = {mode: session.execute('FETCH FORWARD 512 FROM identities;').splitlines()
                   for mode, session in sessions.items()}
        oracle = batches['oracle']
        if len(oracle) > 512 or any(batch != oracle for batch in batches.values()):
            raise ValueError(f'{case}: candidate path and heap oracle disagree on CTIDs')
        count += len(oracle)
        if len(oracle) < 512:
            break
    for session in sessions.values():
        session.execute('CLOSE identities;')
    return count


@dataclass(frozen=True)
class ProcStat:
    pid: int
    start: int
    user: int
    system: int
    minor: int
    major: int
    rss_pages: int


def parse_proc_stat(text: str, pid: int) -> ProcStat:
    # comm may contain spaces and right parentheses; field 3 follows the final one.
    left, right = text.find('('), text.rfind(')')
    if left < 1 or right < left or int(text[:left]) != pid:
        raise ValueError('unexpected proc PID')
    if not text[left + 1:right].startswith('postgres'):
        raise ValueError('proc task is not a PostgreSQL backend')
    fields = text[right + 1:].split()
    if len(fields) < 22:
        raise ValueError('truncated proc stat')
    values = [int(fields[index]) for index in (19, 11, 12, 7, 9, 21)]
    if any(value < 0 for value in values) or values[0] == 0:
        raise ValueError('invalid proc counter')
    return ProcStat(pid, *values)


def cpu_delta(before: ProcStat, after: ProcStat, ticks: int, queries: int) -> dict:
    if (before.pid, before.start) != (after.pid, after.start):
        raise ValueError('backend PID was reused during measurement')
    if ticks <= 0 or queries <= 0:
        raise ValueError('invalid CPU denominator')
    deltas = {name: getattr(after, name) - getattr(before, name)
              for name in ('user', 'system', 'minor', 'major')}
    if any(value < 0 for value in deltas.values()):
        raise ValueError('backend counter moved backwards')
    total = deltas['user'] + deltas['system']
    return {'user_ticks': deltas['user'], 'system_ticks': deltas['system'],
            'tick_ms': 1000 / ticks, 'cpu_ms_per_query': total * 1000 / ticks / queries,
            'resolution_warning': total < 100,
            'minor_faults': deltas['minor'], 'major_faults': deltas['major'],
            'rss_pages_before': before.rss_pages, 'rss_pages_after': after.rss_pages}


def read_cpu(proc: Path | None, pid: int) -> tuple[ProcStat | None, str | None]:
    if proc is None:
        return None, 'not requested; requires database host and matching PID namespace'
    try:
        return parse_proc_stat((proc / str(pid) / 'stat').read_text(), pid), None
    except (OSError, ValueError) as error:
        return None, str(error)


def percentiles(values: list[float]) -> dict:
    if not values or any(not math.isfinite(value) or value < 0 for value in values):
        raise ValueError('finite nonnegative latency samples required')
    ordered = sorted(values)
    return {'n': len(values), **{f'p{p}_ms': ordered[math.ceil(len(ordered) * p / 100) - 1]
                               for p in (50, 95, 99)},
            'min_ms': ordered[0], 'max_ms': ordered[-1]}


def orders(samples: int):
    if not 6 <= samples <= 60 or samples % 6:
        raise ValueError('samples must be a multiple of six in 6..60')
    permutations = tuple(itertools.permutations(MODES))
    return [permutations[index % 6] for index in range(samples)]


def save(path: Path, value: object) -> None:
    pending = path.with_suffix(path.suffix + '.tmp')
    pending.write_text(json.dumps(value, indent=2, allow_nan=False) + '\n')
    pending.replace(path)


def measure(args: argparse.Namespace) -> None:
    psql = args.bindir / 'psql'
    results = []
    with ExitStack() as stack:
        sessions = {mode: stack.enter_context(Session(
                        psql, args.output / f'{mode}.stderr', mode,
                        frontier_anchors=args.frontier_anchors,
                        owner_frontier=args.owner_frontier))
                    for mode in ('oracle', *MODES)}
        oracle = sessions['oracle']
        oracle.execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
        snapshot = oracle.execute('SELECT pg_export_snapshot();')
        if not re.fullmatch(r'[0-9A-Fa-f-]{1,128}', snapshot):
            raise ValueError('invalid exported snapshot identifier')
        for mode in MODES:
            sessions[mode].execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
            sessions[mode].execute(f'SET TRANSACTION SNAPSHOT {literal(snapshot)};')
        # no query may precede snapshot import on the comparison connections.
        anchors = {mode: frontier_options.require_setting(
                       session.execute(frontier_options.PSQL_SETTING_SQL), args.frontier_anchors)
                   for mode, session in sessions.items()}
        owners = {mode: owner_frontier_options.require_setting(
                      session.execute(owner_frontier_options.PSQL_SETTING_SQL), args.owner_frontier)
                  for mode, session in sessions.items()}
        environment = json.loads(oracle.execute(
            "SELECT json_object_agg(name, setting) FROM pg_settings WHERE name IN "
            "('server_version','server_version_num','shared_buffers','work_mem',"
            "'fsync','full_page_writes','synchronous_commit','block_size','track_io_timing');"))
        if environment.get('server_version_num') != '180006':
            raise ValueError('this qualification requires the pinned PostgreSQL 18.6')
        if any(environment.get(key) != 'on' for key in ('fsync', 'full_page_writes')):
            raise ValueError('durability settings are disabled')
        pids = {mode: int(session.execute('SELECT pg_backend_pid();'))
                for mode, session in sessions.items()}
        settings = {mode: session.options for mode, session in sessions.items()}
        save(args.output / 'environment.json', {
            'server_settings': environment, 'session_options': settings, 'backend_pids': pids,
            'registered_frontier_anchors': anchors,
            'registered_owner_frontier': owners,
            'server_build_revision': oracle.execute('SELECT pin.build_revision();'),
            'client_platform': platform.platform(), 'client_cpu_count': os.cpu_count(),
            'same_host_proc_asserted': str(args.backend_proc) if args.backend_proc else None,
            'host_note': args.host_note, 'samples': args.samples, 'queries': args.queries,
            'warmup': args.warmup, 'snapshot': snapshot,
            'scope': 'warm serial prepared count, fixed snapshot, includes heap/MVCC/recheck',
            'grouped_path': 'enabled setting, not proof of selected path; fallback is permitted',
            'not_measured': ['hardware counters', 'sampled stacks', 'allocation profile',
                             'cold cache', 'concurrent throughput', 'write latency', 'ranking'],
        })
        ticks = os.sysconf('SC_CLK_TCK') if args.backend_proc else 1
        with (args.output / 'samples.jsonl').open('x') as journal:
            for case in args.cases:
                for mode, session in sessions.items():
                    query = f'SELECT count(*) FROM {TABLE} WHERE {predicate(case, mode)};'
                    session.execute('PREPARE measured AS ' + query)
                    plan = json.loads(session.execute('EXPLAIN (FORMAT JSON) EXECUTE measured;'))
                    inspect_plan(plan, mode)
                    save(args.output / f'{case}-{mode}-planned.json', plan)
                    (args.output / f'{case}-{mode}.sql').write_text(query + '\n')
                count = matching_rows(sessions, case)
                save(args.output / f'{case}-identities.json', {'rows': count, 'exact_ctids': True})
                for mode in MODES:
                    session = sessions[mode]
                    for _ in range(args.warmup):
                        if session.execute('EXECUTE measured;') != str(count):
                            raise ValueError('warmup result changed')
                    # timing-on instrumentation is separate from the latency/CPU samples.
                    plan = json.loads(session.execute(
                        'EXPLAIN (ANALYZE, BUFFERS, WAL, SETTINGS, TIMING ON, FORMAT JSON) '
                        'EXECUTE measured;'))
                    inspect_plan(plan, mode)
                    save(args.output / f'{case}-{mode}-timed-plan.json', plan)
                for sample, order in enumerate(orders(args.samples)):
                    for position, mode in enumerate(order):
                        session = sessions[mode]
                        before, before_error = read_cpu(args.backend_proc, pids[mode])
                        latency = []
                        for _ in range(args.queries):
                            start = time.perf_counter_ns()
                            result = session.execute('EXECUTE measured;')
                            latency.append((time.perf_counter_ns() - start) / 1e6)
                            if result != str(count):
                                raise ValueError('measured count differs from verified identities')
                        after, after_error = read_cpu(args.backend_proc, pids[mode])
                        cpu = cpu_delta(before, after, ticks, args.queries) if before and after else None
                        plan = json.loads(session.execute(
                            'EXPLAIN (ANALYZE, BUFFERS, WAL, SETTINGS, TIMING OFF, FORMAT JSON) '
                            'EXECUTE measured;'))
                        work = inspect_plan(plan, mode)
                        save(args.output / f'{case}-{mode}-{sample}-plan.json', plan)
                        record = {'case': case, 'mode': mode, 'sample': sample, 'position': position,
                                  'client_roundtrip': percentiles(latency), 'client_ms': latency,
                                  'backend_cpu': cpu, 'cpu_unavailable': before_error or after_error,
                                  'separate_explain_probe': work}
                        journal.write(json.dumps(record, allow_nan=False) + '\n')
                        journal.flush()
                        results.append({key: value for key, value in record.items() if key != 'client_ms'})
                        save(args.output / 'results.json', results)
                        print(f'{case} {mode} sample={sample} rows={count}', flush=True)
                for session in sessions.values():
                    session.execute('DEALLOCATE measured;')
        save(args.output / 'status.json', {'status': 'complete', 'batches': len(results)})


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bindir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--frontier-anchors', action='store_true')
    parser.add_argument('--owner-frontier', action='store_true')
    parser.add_argument('--samples', type=int, default=6)
    parser.add_argument('--queries', type=int, default=100)
    parser.add_argument('--warmup', type=int, default=5)
    parser.add_argument('--cases', nargs='+', choices=CASES, default=list(CASES))
    parser.add_argument('--backend-proc', type=Path,
                        help='assert this is the DATABASE HOST procfs in its PID namespace')
    parser.add_argument('--host-note', default='isolation and hardware not documented')
    args = parser.parse_args()
    try:
        orders(args.samples)
        if not 1 <= args.queries <= 10000 or not 0 <= args.warmup <= 1000:
            raise ValueError('queries must be 1..10000 and warmup 0..1000')
        if len(args.cases) != len(set(args.cases)):
            raise ValueError('duplicate cases are not supported')
    except ValueError as error:
        parser.error(str(error))
    args.output.mkdir(parents=True, exist_ok=False)
    save(args.output / 'status.json', {'status': 'incomplete'})
    try:
        measure(args)
    except BaseException as error:
        save(args.output / 'status.json', {'status': 'failed', 'error': str(error)})
        raise


if __name__ == '__main__':
    main()
