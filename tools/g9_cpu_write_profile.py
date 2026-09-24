#!/usr/bin/env python3
"""Profile synthetic Pin and GIN inserts in a disposable PostgreSQL cluster."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time

import psycopg2
import psutil

from g9_cpu_profile import delta, proc_snapshot


def cluster_snapshot(postmaster_pid: int) -> dict[int, dict]:
    processes = [psutil.Process(postmaster_pid)]
    processes.extend(processes[0].children(recursive=True))
    result = {}
    for process in processes:
        try:
            result[process.pid] = {
                'command': ' '.join(process.cmdline()),
                'counters': proc_snapshot(process.pid),
            }
        except (OSError, psutil.Error):
            continue
    return result


def render(data: Path) -> None:
    for kind, order in (('dso', 'dso'), ('flat', 'dso,symbol'),
                        ('stacks', 'dso,symbol')):
        command = [
            'sudo', '-n', 'perf', 'report', '--stdio', '--no-children',
            '--sort', order, '--percent-limit', '0', '-i', str(data),
        ]
        command += ['--call-graph', 'graph' if kind == 'stacks' else 'none']
        result = subprocess.run(command, capture_output=True, text=True,
                                timeout=60, check=True)
        data.with_suffix(f'.{kind}.txt').write_text(result.stdout)


def measure(cur, rows: int, repeats: int, label: str, engine: str, sample: int,
            output: Path, postmaster_pid: int) -> dict:
    name = f'pin_cpu_write_{label}_{engine}_{sample}'
    cur.execute(f'CREATE TABLE public.{name}(id bigint PRIMARY KEY, body text NOT NULL)')
    if engine == 'pin':
        cur.execute(f'CREATE INDEX {name}_body ON public.{name} '
                    'USING pin(body pin.text_ops)')
    else:
        cur.execute(f'CREATE INDEX {name}_body ON public.{name} '
                    "USING gin(to_tsvector('simple', body))")
    cur.execute('SELECT pg_backend_pid(), pg_current_wal_insert_lsn()')
    pid, start_lsn = cur.fetchone()
    data = output / f'{name}-perf.data'
    recorder = subprocess.Popen(
        ['sudo', '-n', 'perf', 'record', '--quiet', '-e', 'cpu-clock',
         '-F', '199', '-g', '--call-graph', 'dwarf,8192', '-p', str(pid),
         '-o', str(data)],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        start_new_session=True,
    )
    sql = (f'INSERT INTO public.{name}(id, body) '
           f"SELECT i, repeat('alpha beta gamma ', {repeats}) || (i % 100)::text "
           f'FROM generate_series(1, {rows}) AS i')
    try:
        time.sleep(0.25)
        cluster_before = cluster_snapshot(postmaster_pid)
        before = proc_snapshot(pid)
        started = time.perf_counter_ns()
        cur.execute(sql)
        elapsed = time.perf_counter_ns() - started
        work = delta(before, proc_snapshot(pid))
        time.sleep(0.5)
        cluster_after = cluster_snapshot(postmaster_pid)
    finally:
        if recorder.poll() is None:
            os.killpg(recorder.pid, signal.SIGINT)
        recorder.wait(timeout=15)
        stderr = recorder.stderr.read()
    if not data.is_file() or data.stat().st_size == 0:
        raise RuntimeError(f'empty perf recording: {stderr}')
    render(data)
    cur.execute('SELECT pg_current_wal_insert_lsn()')
    end_lsn = cur.fetchone()[0]
    cur.execute('SELECT pg_wal_lsn_diff(%s, %s)', (end_lsn, start_lsn))
    wal_bytes = int(cur.fetchone()[0])
    cur.execute(f'SELECT count(*) FROM public.{name}')
    if cur.fetchone()[0] != rows:
        raise RuntimeError('insert row count mismatch')
    cur.execute(f"SELECT pg_relation_size('public.{name}'::regclass), "
                f"pg_relation_size('public.{name}_body'::regclass)")
    table_bytes, index_bytes = cur.fetchone()
    cluster_work = {
        str(process_pid): {
            'command': row['command'],
            'counters': delta(row['counters'], cluster_after[process_pid]['counters']),
        }
        for process_pid, row in cluster_before.items()
        if process_pid in cluster_after
    }
    return {
        'engine': engine,
        'sample': sample,
        'table': name,
        'rows': rows,
        'text_repeats': repeats,
        'sql': sql,
        'backend_cpu_ns': work['on_cpu_ns'],
        'backend_cpu_us_per_row': work['on_cpu_ns'] / rows / 1000,
        'wall_ns': elapsed,
        'runqueue_ns': work['runqueue_ns'],
        'wal_bytes_heap_and_index': wal_bytes,
        'table_bytes': table_bytes,
        'index_bytes': index_bytes,
        'proc': work,
        'cluster_process_counters_including_0_5s_settle': cluster_work,
        'perf_data': str(data),
        'perf_stderr': stderr.strip(),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--rows', type=int, default=15000)
    parser.add_argument('--repeats', type=int, default=32)
    parser.add_argument('--samples', type=int, default=2)
    parser.add_argument('--label', default='run')
    parser.add_argument('--disposable', action='store_true', required=True)
    args = parser.parse_args()
    if not 1000 <= args.rows <= 100000 or not 1 <= args.repeats <= 128:
        parser.error('rows must be 1000..100000 and repeats 1..128')
    if not 1 <= args.samples <= 5:
        parser.error('samples must be 1..5')
    if re.fullmatch(r'[a-z][a-z0-9_]{0,12}', args.label) is None:
        parser.error('label must be 1..13 lowercase letters, digits or underscores')
    args.output.mkdir(parents=True, exist_ok=False)
    conn = psycopg2.connect(application_name='pin_g9_cpu_write_profile')
    conn.autocommit = True
    cur = conn.cursor()
    cur.execute("SELECT current_setting('data_directory'), current_setting('fsync'), "
                "current_setting('synchronous_commit'), pin.build_revision()")
    data_directory, fsync, synchronous_commit, revision = cur.fetchone()
    if not data_directory.startswith('/tmp/pin-g9-'):
        raise RuntimeError('write profiling requires a /tmp/pin-g9-* cluster')
    postmaster_pid = int((Path(data_directory) / 'postmaster.pid').read_text().splitlines()[0])
    environment = {
        'data_directory': data_directory,
        'fsync': fsync,
        'synchronous_commit': synchronous_commit,
        'server_build_revision': revision,
        'postmaster_pid': postmaster_pid,
        'gin_fastupdate': 'default on',
        'note': 'each table has a primary key and one text index',
    }
    (args.output / 'environment.json').write_text(json.dumps(environment, indent=2) + '\n')
    results = []
    try:
        for sample in range(args.samples):
            engines = ('pin', 'gin') if sample % 2 == 0 else ('gin', 'pin')
            for engine in engines:
                result = measure(cur, args.rows, args.repeats, args.label, engine, sample,
                                 args.output, postmaster_pid)
                results.append(result)
                (args.output / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
                print(engine, sample, round(result['backend_cpu_us_per_row'], 2),
                      'backend CPU us/row', flush=True)
    finally:
        conn.close()


if __name__ == '__main__':
    main()
