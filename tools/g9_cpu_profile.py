#!/usr/bin/env python3
"""Measure one PostgreSQL backend's CPU cost for fixed Pin and GIN queries.

Run against a disposable G6 comparison fixture. Requires psycopg2. Hardware PMU
events are optional; this tool uses Linux schedstat for on-CPU nanoseconds.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import time

if __package__:
    from . import frontier_options
    from .g9_profile import CASES
else:
    import frontier_options
    from g9_profile import CASES

BASE_SETTINGS = (
    'SET enable_seqscan = off',
    'SET enable_bitmapscan = on',
    'SET enable_indexscan = off',
    'SET enable_indexonlyscan = off',
    'SET max_parallel_workers_per_gather = 0',
    'SET jit = off',
    'SET pin.enable_count_fastpath = off',
    'SET pin.enable_count_vm = off',
    'SET pin.enable_exact_bitmap = on',
    "SET statement_timeout = '120s'",
)


def predicate(case: str, engine: str) -> str:
    pin, gin = CASES[case]
    if engine == 'gin':
        return f"to_tsvector('simple', body) @@ to_tsquery('simple', '{gin}')"
    return f"body OPERATOR(pin.@@@) pin.parse_query('{pin}')"


def proc_snapshot(pid: int) -> dict[str, int]:
    root = Path(f'/proc/{pid}')
    runtime, runqueue, slices = map(int, (root / 'schedstat').read_text().split())
    stat = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    status = (root / 'status').read_text().splitlines()
    context = {
        line.split(':', 1)[0]: int(line.split(':', 1)[1].strip())
        for line in status
        if line.startswith(('voluntary_ctxt_switches:', 'nonvoluntary_ctxt_switches:'))
    }
    io = {}
    for line in (root / 'io').read_text().splitlines():
        key, value = line.split(':', 1)
        if key in ('read_bytes', 'write_bytes', 'syscr', 'syscw'):
            io[key] = int(value.strip())
    return {
        'start_ticks': int(stat[19]),
        'on_cpu_ns': runtime,
        'runqueue_ns': runqueue,
        'timeslices': slices,
        'user_ticks': int(stat[11]),
        'system_ticks': int(stat[12]),
        'minor_faults': int(stat[7]),
        'major_faults': int(stat[9]),
        **context,
        **io,
    }


def delta(before: dict[str, int], after: dict[str, int]) -> dict[str, int]:
    if before['start_ticks'] != after['start_ticks']:
        raise RuntimeError('backend pid was reused')
    result = {key: after[key] - value for key, value in before.items() if key != 'start_ticks'}
    if any(value < 0 for value in result.values()):
        raise RuntimeError('backend counters moved backwards')
    return result


def connect(engine: str, *, anchors: bool = False):
    import psycopg2
    conn = psycopg2.connect(application_name='pin_g9_cpu_profile', connect_timeout=10)
    conn.autocommit = True
    try:
        cur = conn.cursor()
        for setting in BASE_SETTINGS:
            cur.execute(setting)
        cur.execute(f'SET pin.enable_grouped_scan = {"on" if engine == "grouped" else "off"}')
        cur.execute(f'SET {frontier_options.NAME} = {"on" if anchors else "off"}')
        cur.execute(frontier_options.SETTING_SQL)
        row = cur.fetchone()
        frontier_options.require_setting(row[0] if row else None, anchors)
        return conn, cur
    except BaseException:
        conn.close()
        raise


def prepare(cur, case: str, engine: str) -> dict:
    cur.execute('DEALLOCATE ALL')
    cur.execute('PREPARE cpu_query AS SELECT count(*) FROM ONLY public.pin_g6_bench '
                f'WHERE {predicate(case, engine)}')
    cur.execute('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) EXECUTE cpu_query')
    plan = cur.fetchone()[0][0]
    node = plan['Plan']
    bitmap = None
    heap = None
    stack = [node]
    while stack:
        item = stack.pop()
        if item['Node Type'] == 'Bitmap Index Scan':
            bitmap = item
        if item['Node Type'] == 'Bitmap Heap Scan':
            heap = item
        stack.extend(item.get('Plans', []))
    expected = ('pin_g6_bench_body_gin' if engine == 'gin' else 'pin_g6_bench_body')
    if bitmap is None or bitmap.get('Index Name') != expected or heap is None:
        raise RuntimeError(f'{case}/{engine}: unexpected plan: {plan}')
    return {
        'explain': plan,
        'sql': f'SELECT count(*) FROM ONLY public.pin_g6_bench WHERE {predicate(case, engine)}',
        'index_name': bitmap['Index Name'],
        'index_ms': bitmap['Actual Total Time'],
        'index_rows': bitmap['Actual Rows'],
        'index_hits': bitmap.get('Shared Hit Blocks', 0),
        'index_reads': bitmap.get('Shared Read Blocks', 0),
        'heap_ms': heap['Actual Total Time'],
        'heap_hits': heap.get('Shared Hit Blocks', 0),
        'heap_reads': heap.get('Shared Read Blocks', 0),
        'heap_exact_blocks': heap.get('Exact Heap Blocks', 0),
        'heap_lossy_blocks': heap.get('Lossy Heap Blocks', 0),
        'heap_recheck_rows': heap.get('Rows Removed by Index Recheck', 0),
        'total_ms': node['Actual Total Time'],
    }


def run_batch(cur, pid: int, seconds: float) -> dict:
    before = proc_snapshot(pid)
    started = time.perf_counter_ns()
    deadline = started + int(seconds * 1e9)
    count = 0
    result = None
    while time.perf_counter_ns() < deadline:
        cur.execute('EXECUTE cpu_query')
        value = cur.fetchone()[0]
        if result is not None and value != result:
            raise RuntimeError('result count changed during a read-only sample')
        result = value
        count += 1
    elapsed = time.perf_counter_ns() - started
    work = delta(before, proc_snapshot(pid))
    if count == 0 or work['on_cpu_ns'] <= 0:
        raise RuntimeError('sample did not execute or record backend CPU time')
    return {
        'queries': count,
        'matching_rows': result,
        'elapsed_ns': elapsed,
        'backend': work,
        'backend_cpu_us_per_query': work['on_cpu_ns'] / count / 1000,
        'runqueue_us_per_query': work['runqueue_ns'] / count / 1000,
        'wall_us_per_query': elapsed / count / 1000,
        'qps_single_client': count * 1e9 / elapsed,
        'backend_cpu_fraction_of_wall': work['on_cpu_ns'] / elapsed,
    }


def profile(cur, pid: int, seconds: float, output: Path) -> dict:
    data = output.with_suffix('.data')
    report = output.with_suffix('.txt')
    command = [
        'sudo', '-n', 'perf', 'record', '--quiet', '-e', 'cpu-clock', '-F', '199',
        '-g', '--call-graph', 'dwarf,8192', '-p', str(pid), '-o', str(data),
        '--', 'sleep', str(seconds + 1),
    ]
    recorder = subprocess.Popen(command, stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE, text=True)
    try:
        time.sleep(0.25)
        sample = run_batch(cur, pid, seconds)
        stderr = recorder.communicate(timeout=seconds + 15)[1]
        if recorder.returncode != 0:
            raise RuntimeError(f'perf record failed: {stderr}')
        result = subprocess.run(
            ['sudo', '-n', 'perf', 'report', '--stdio', '--no-children',
             '--sort', 'dso,symbol', '-i', str(data)],
            check=True, capture_output=True, text=True, timeout=60,
        )
        report.write_text(result.stdout)
        sample['perf_data'] = str(data)
        sample['perf_report'] = str(report)
        sample['perf_record_stderr'] = stderr.strip()
        return sample
    finally:
        if recorder.poll() is None:
            recorder.terminate()
            recorder.wait(timeout=10)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--frontier-anchors', action='store_true')
    parser.add_argument('--seconds', type=float, default=3)
    parser.add_argument('--samples', type=int, default=3)
    parser.add_argument('--cases', nargs='+', choices=CASES, default=list(CASES))
    parser.add_argument('--profile-seconds', type=float, default=0)
    parser.add_argument('--profile-cases', nargs='+', choices=CASES,
                        default=['rare', 'and', 'selective_and', 'phrase'])
    args = parser.parse_args()
    if not 0.5 <= args.seconds <= 120 or not 1 <= args.samples <= 20:
        parser.error('seconds must be 0.5..120 and samples 1..20')
    if not 0 <= args.profile_seconds <= 120:
        parser.error('profile-seconds must be 0..120')
    args.output.mkdir(parents=True, exist_ok=False)
    engines = ('legacy', 'grouped', 'gin')
    rows = []
    pmu_command = [
        'sudo', '-n', 'perf', 'stat', '-e',
        'cycles,instructions,cache-misses,branches,branch-misses', '--', 'true',
    ]
    try:
        pmu_probe = subprocess.run(pmu_command, capture_output=True, text=True,
                                   timeout=15, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        pmu_probe = subprocess.CompletedProcess(pmu_command, 127, '', repr(error))
    environment = {
        'platform': platform.platform(),
        'cpu_model': next((line.split(':', 1)[1].strip()
                           for line in Path('/proc/cpuinfo').read_text().splitlines()
                           if line.startswith('model name')), None),
        'kernel_perf_event_paranoid': Path('/proc/sys/kernel/perf_event_paranoid').read_text().strip(),
        'cpu_count': os.cpu_count(),
        'revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'seconds': args.seconds,
        'samples': args.samples,
        'pmu_probe': {
            'command': pmu_command,
            'exit_code': pmu_probe.returncode,
            'stderr': pmu_probe.stderr,
        },
        'hardware_pmu_counters_collected': False,
        'metric': '/proc/PID/schedstat backend on-CPU nanoseconds',
    }
    conn, cur = connect('legacy', anchors=args.frontier_anchors)
    try:
        cur.execute("SELECT json_object_agg(name, setting) FROM pg_settings WHERE name IN "
                    "('server_version', 'shared_buffers', 'work_mem', 'fsync', "
                    "'full_page_writes', 'synchronous_commit', 'block_size', 'pin.enable_frontier_anchors')")
        environment['postgres_settings'] = cur.fetchone()[0]
        cur.execute("SELECT relname, pg_relation_size(oid) FROM pg_class WHERE oid IN "
                    "('public.pin_g6_bench'::regclass, "
                    "'public.pin_g6_bench_body'::regclass, "
                    "'public.pin_g6_bench_body_gin'::regclass)")
        environment['relations_bytes'] = dict(cur.fetchall())
        cur.execute('SELECT count(*) FROM public.pin_g6_bench')
        environment['documents'] = cur.fetchone()[0]
    finally:
        conn.close()
    (args.output / 'environment.json').write_text(json.dumps(environment, indent=2) + '\n')
    for case in args.cases:
        conn, cur = connect('legacy', anchors=args.frontier_anchors)
        try:
            pin_rows = 'SELECT id FROM ONLY public.pin_g6_bench WHERE ' + predicate(case, 'legacy')
            gin_rows = 'SELECT id FROM ONLY public.pin_g6_bench WHERE ' + predicate(case, 'gin')
            cur.execute(f'SELECT count(*) FROM (({pin_rows} EXCEPT ALL {gin_rows}) '
                        f'UNION ALL ({gin_rows} EXCEPT ALL {pin_rows})) AS difference')
            if cur.fetchone()[0] != 0:
                raise RuntimeError(f'{case}: Pin and GIN row identities differ')
        finally:
            conn.close()
        counts = {}
        for engine in engines:
            conn, cur = connect(engine, anchors=args.frontier_anchors)
            try:
                cur.execute('SELECT pg_backend_pid()')
                pid = cur.fetchone()[0]
                plan = prepare(cur, case, engine)
                cur.execute('EXECUTE cpu_query')
                counts[engine] = cur.fetchone()[0]
                cur.execute('SELECT pin.build_revision()')
                revision = cur.fetchone()[0]
                rows.append({'case': case, 'engine': engine, 'pid': pid,
                             'plan': plan, 'server_build_revision': revision,
                             'samples': [], 'profile': None})
            finally:
                conn.close()
        if len(set(counts.values())) != 1:
            raise RuntimeError(f'{case}: result count differs: {counts}')
    (args.output / 'plans.json').write_text(json.dumps(rows, indent=2) + '\n')
    for sample_index in range(args.samples):
        order = rows if sample_index % 2 == 0 else list(reversed(rows))
        for row in order:
            conn, cur = connect(row['engine'], anchors=args.frontier_anchors)
            try:
                pid = conn.get_backend_pid()
                prepare(cur, row['case'], row['engine'])
                run_batch(cur, pid, min(0.5, args.seconds))
                sample = run_batch(cur, pid, args.seconds)
                row['samples'].append(sample)
                print(row['case'], row['engine'], sample_index,
                      round(sample['backend_cpu_us_per_query'], 2), 'CPU us/query', flush=True)
            finally:
                conn.close()
        (args.output / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
    if args.profile_seconds:
        for row in rows:
            if row['case'] not in args.profile_cases:
                continue
            conn, cur = connect(row['engine'], anchors=args.frontier_anchors)
            try:
                pid = conn.get_backend_pid()
                prepare(cur, row['case'], row['engine'])
                run_batch(cur, pid, 0.5)
                row['profile'] = profile(
                    cur, pid, args.profile_seconds,
                    args.output / f"{row['case']}-{row['engine']}-perf",
                )
                print('profile', row['case'], row['engine'], flush=True)
            finally:
                conn.close()
            (args.output / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')


if __name__ == '__main__':
    main()
