#!/usr/bin/env python3
"""Qualify fresh, growing-delta and maintained PIN/GIN workloads on a disposable host.

Creates a new fixture, never drops an existing relation, checks the loaded binary
revision, and retains every child command and log. Requires psycopg2 and Linux
/proc access to the server backend. Count, row retrieval, concurrent writes and
maintenance observations are separate records, never one aggregate speedup.
"""
from __future__ import annotations

import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import threading
import time

from g9_profile import CASES, INDEX, MODES, SETTINGS, literal
from g6_latency import summarize
import frontier_options

TABLE = 'public.pin_g6_bench'
WRITE_TABLES = {'pin': 'public.pin_frontier_write_pin', 'gin': 'public.pin_frontier_write_gin'}


def save(path: Path, value: object) -> None:
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2, allow_nan=False) + '\n')
    temporary.replace(path)


def validate_deltas(values: list[int]) -> None:
    if not values or values[0] != 0 or values != sorted(set(values)) or values[-1] > 1_000_000:
        raise ValueError('deltas must start at zero and strictly increase through at most 1000000')


def validate_host(settings: dict, revision: str, expected: str, data: Path) -> None:
    if not re.fullmatch(r'[0-9a-f]{40}', expected) or revision != expected:
        raise ValueError('loaded pin.build_revision() must equal the full requested commit')
    if settings.get('server_version_num') != '180006' or settings.get('block_size') != '8192':
        raise ValueError('the qualified server is PostgreSQL 18.6 with 8192-byte pages')
    if any(settings.get(name) != 'on' for name in ('fsync', 'full_page_writes', 'synchronous_commit')):
        raise ValueError('fsync, full_page_writes and synchronous_commit must be on')
    if data.parent != Path('/tmp') or not data.name.startswith('pin-g9-') or data.is_symlink():
        raise ValueError('only a local /tmp/pin-g9-* disposable cluster is accepted')
    if not (data / 'postmaster.pid').is_file():
        raise ValueError('local postmaster.pid is required')


def predicate(case: str, mode: str) -> str:
    pin, gin = CASES[case]
    if mode == 'gin':
        return f"to_tsvector('simple', body) @@ to_tsquery('simple', {literal(gin)})"
    return f'body OPERATOR(pin.@@@) pin.parse_query({literal(pin)})'


def connection(mode: str = 'pin_grouped_enabled', *, anchors: bool = False):
    import psycopg2
    conn = psycopg2.connect(application_name='pin_frontier_qualification', connect_timeout=10)
    conn.autocommit = True
    try:
        with conn.cursor() as cur:
            for name, value in frontier_options.options(SETTINGS, anchors).items():
                cur.execute(f'SET {name} = {literal(value)}')
            cur.execute('SET pin.enable_grouped_scan = ' + ('off' if mode in ('pin_legacy', 'gin') else 'on'))
            cur.execute(frontier_options.SETTING_SQL)
            row = cur.fetchone()
            frontier_options.require_setting(row[0] if row else None, anchors)
        return conn
    except BaseException:
        conn.close()
        raise


def proc(pid: int) -> dict:
    root = Path('/proc') / str(pid)
    identity, fields = (root / 'stat').read_text().rsplit(')', 1)
    if not identity.endswith('(postgres'):
        raise RuntimeError('pid does not identify a postgres backend')
    raw = fields.split()
    runtime = (root / 'schedstat').read_text().split()
    if len(runtime) != 3 or int(runtime[0]) <= 0:
        raise RuntimeError('backend schedstat on-cpu accounting is unavailable')
    io = dict(line.split(':', 1) for line in (root / 'io').read_text().splitlines())
    return {'start_ticks': int(raw[19]), 'on_cpu_ns': int(runtime[0]),
            'runqueue_ns': int(runtime[1]), 'minor_faults': int(raw[7]),
            'major_faults': int(raw[9]),
            **{name: int(io[name]) for name in ('read_bytes', 'write_bytes', 'syscr', 'syscw')}}


def difference(before: dict, after: dict) -> dict:
    if before['start_ticks'] != after['start_ticks']:
        raise RuntimeError('backend pid was reused')
    result = {key: after[key] - value for key, value in before.items() if key != 'start_ticks'}
    if any(value < 0 for value in result.values()):
        raise RuntimeError('backend counters moved backwards')
    return result


def measured(cur, statement: str) -> dict:
    cur.execute('SELECT pg_backend_pid(), pg_current_wal_insert_lsn()::text')
    pid, start_lsn = cur.fetchone()
    before = proc(pid)
    start = time.perf_counter_ns()
    cur.execute(statement)
    result = cur.fetchone() if cur.description is not None else None
    elapsed = time.perf_counter_ns() - start
    after = proc(pid)
    cur.execute('SELECT pg_wal_lsn_diff(pg_current_wal_insert_lsn(), %s)::bigint', (start_lsn,))
    return {'sql': statement, 'result': result, 'elapsed_ns': elapsed, 'backend': difference(before, after),
            'cluster_wal_bytes': cur.fetchone()[0]}


def verify(cur, cases: list[str]) -> None:
    for case in cases:
        oracle = (f'SELECT id, ctid FROM ONLY {TABLE} WHERE '
                  f"to_tsvector('simple', body) @@ to_tsquery('simple', {literal(CASES[case][1])})")
        for mode in MODES:
            cur.execute('SET pin.enable_grouped_scan = ' + ('on' if mode == MODES[0] else 'off'))
            rows = f'SELECT id, ctid FROM ONLY {TABLE} WHERE {predicate(case, mode)}'
            cur.execute(f'SELECT count(*) FROM (({rows} EXCEPT ALL {oracle}) '
                        f'UNION ALL ({oracle} EXCEPT ALL {rows})) AS difference')
            if cur.fetchone()[0] != 0:
                raise RuntimeError(f'{case}/{mode}: row identities differ')
            cur.execute(f'EXPLAIN (FORMAT JSON) {rows}')
            stack = [cur.fetchone()[0][0]['Plan']]
            found = []
            while stack:
                node = stack.pop()
                if node['Node Type'] == 'Bitmap Index Scan':
                    found.append(node.get('Index Name'))
                stack.extend(node.get('Plans', []))
            if found != [INDEX[mode]]:
                raise RuntimeError(f'{case}/{mode}: expected bitmap plan not selected: {found}')
    cur.execute('SET pin.enable_grouped_scan = on')


def retrieve(conn, case: str, mode: str) -> dict:
    # stream every identity and text byte; this is not an index-only count sample.
    with conn.cursor() as cur:
        cur.execute('SET pin.enable_grouped_scan = ' + ('on' if mode == MODES[0] else 'off'))
        cur.execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY')
        query = f'SELECT id, body FROM ONLY {TABLE} WHERE {predicate(case, mode)}'
        cur.execute('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) ' + query)
        plan = cur.fetchone()[0][0]
        cur.execute('DECLARE frontier_rows NO SCROLL CURSOR FOR ' + query)
        pid = conn.get_backend_pid()
        before = proc(pid)
        start = time.perf_counter_ns()
        count = byte_count = 0
        while True:
            cur.execute('FETCH FORWARD 256 FROM frontier_rows')
            batch = cur.fetchall()
            if not batch:
                break
            count += len(batch)
            byte_count += sum(len(body.encode('utf-8')) for _, body in batch)
        elapsed = time.perf_counter_ns() - start
        after = proc(pid)
        cur.execute('COMMIT')
    return {'case': case, 'mode': mode, 'rows': count, 'utf8_body_bytes': byte_count,
            'elapsed_ns': elapsed, 'backend': difference(before, after), 'plan': plan,
            'measurement': 'one cursor drain after explain; excludes cursor declaration and commit'}


def child(output: Path, name: str, command: list[str], timeout: float = 3600,
          *, anchors: bool = False) -> None:
    command = [*command, *(['--frontier-anchors'] if anchors else [])]
    save(output / (name + '-command.json'), command)
    with (output / (name + '-runner.log')).open('x') as log:
        settings = frontier_options.options(SETTINGS, anchors)
        env = dict(os.environ, PGOPTIONS=' '.join(f'-c {key}={value}' for key, value in settings.items()))
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=timeout, env=env)


def read_stage(args, name: str) -> None:
    output = args.output / name
    output.mkdir()
    with closing(connection(anchors=args.frontier_anchors)) as conn, conn.cursor() as cur:
        verify(cur, args.cases)
        cur.execute('SELECT count(*), sum(octet_length(body)) FROM ' + TABLE)
        save(output / 'identity.json', {'symmetric_difference': 0, 'fixture': cur.fetchone(),
                                      'cases': args.cases, 'modes': MODES})
        rows = []
        for sample in range(2):
            for case in args.cases:
                for mode in MODES if sample == 0 else reversed(MODES):
                    rows.append(dict(retrieve(conn, case, mode), sample=sample))
        save(output / 'retrieval.json', rows)
    tools = Path(__file__).resolve().parent
    child(output, 'paired', [sys.executable, str(tools / 'g9_profile.py'),
                            '--bindir', str(args.bindir), '--output', str(output / 'paired'),
                            '--samples', '6', '--queries', str(args.queries),
                            '--cases', *args.cases, '--backend-proc', '/proc'],
          anchors=args.frontier_anchors)
    cpu_cases = args.cases
    if cpu_cases:
        child(output, 'cpu', [sys.executable, str(tools / 'g9_cpu_profile.py'),
                             '--output', str(output / 'cpu'), '--samples', str(args.samples),
                             '--seconds', str(args.seconds), '--cases', *cpu_cases,
                             '--profile-seconds', str(args.profile_seconds), '--profile-cases', *cpu_cases],
              anchors=args.frontier_anchors)
    throughput(args, output)


def throughput(args, output: Path) -> None:
    if not args.throughput_seconds:
        return
    results = []
    for sample in range(args.samples):
        for mode in MODES if sample % 2 == 0 else reversed(MODES):
            options = frontier_options.options(SETTINGS, args.frontier_anchors)
            options['pin.enable_grouped_scan'] = 'on' if mode == MODES[0] else 'off'
            env = dict(os.environ, PGOPTIONS=' '.join(f'-c {key}={value}' for key, value in options.items()))
            for case in args.cases:
                script = output / f'{case}-{mode}.sql'
                script.write_text(f'SELECT count(*) FROM ONLY {TABLE} WHERE {predicate(case, mode)};\n')
                prefix = output / f'{case}-{mode}-{sample}.latency'
                command = [str(args.bindir / 'pgbench'), '-n', '-M', 'prepared', '-c', '4', '-j', '2',
                           '-f', str(script), '-T', str(args.throughput_seconds), '-l', '--log-prefix', str(prefix)]
                save(prefix.with_suffix('.command.json'), {'argv': command, 'settings': options})
                result = subprocess.run(command, env=env, capture_output=True, text=True,
                                        check=True, timeout=args.throughput_seconds + 180)
                prefix.with_suffix('.runner.log').write_text(result.stdout + result.stderr)
                tps = re.search(r'^tps = ([\d.]+)', result.stdout, re.MULTILINE)
                failures = re.search(r'number of failed transactions: (\d+)', result.stdout)
                if tps is None or failures is None or int(failures[1]):
                    raise RuntimeError('pgbench did not produce successful transaction metrics')
                results.append({'case': case, 'mode': mode, 'sample': sample, 'clients': 4,
                                'tps': float(tps[1]), 'latency': summarize(output.glob(prefix.name + '.*'))})
                save(output / 'throughput.json', results)


def writes(args, cur) -> None:
    events = []
    for engine, table in WRITE_TABLES.items():
        cur.execute(f'CREATE TABLE {table}(id bigint PRIMARY KEY, body text NOT NULL) '
                    'WITH (autovacuum_enabled = false)')
        method = 'pin(body)' if engine == 'pin' else "gin(to_tsvector('simple', body))"
        cur.execute(f'CREATE INDEX {table.split(".")[1]}_body ON {table} USING {method}')
    for batch in range(args.write_batches):
        for engine in ('pin', 'gin') if batch % 2 == 0 else ('gin', 'pin'):
            table = WRITE_TABLES[engine]
            first = batch * args.write_rows + 1
            last = first + args.write_rows - 1
            insert = measured(cur, f"INSERT INTO {table} SELECT i, repeat('alpha beta gamma ', 32) "
                              f"FROM generate_series({first}, {last}) AS i")
            update = None
            if args.write_update_rows:
                update = measured(cur, f"UPDATE {table} SET body = 'alpha delta rareplanet' "
                                  f"WHERE id BETWEEN {first} AND {first + args.write_update_rows - 1}")
            # pay deferred gin maintenance and the enabled pin snapshot rebuild each round.
            cur.execute('SET pin.enable_grouped_storage = on')
            vacuum = measured(cur, f'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) {table}')
            cur.execute('SET pin.enable_grouped_storage = off')
            cur.execute('SELECT pg_relation_size(%s::regclass), pg_indexes_size(%s::regclass)', (table, table))
            sizes = cur.fetchone()
            phases = [insert, vacuum, *([update] if update is not None else [])]
            events.append({'engine': engine, 'batch': batch, 'inserted_rows': args.write_rows,
                           'updated_rows': args.write_update_rows, 'update': update,
                           'insert': insert, 'maintenance': vacuum, 'heap_and_all_indexes_bytes': sizes,
                           'lifecycle_backend_cpu_ns': sum(phase['backend']['on_cpu_ns'] for phase in phases),
                           'lifecycle_cluster_wal_bytes': sum(phase['cluster_wal_bytes'] for phase in phases)})
            save(args.output / 'write-maintenance.json', events)
    cur.execute(f'SELECT count(*) FROM ((SELECT * FROM {WRITE_TABLES["pin"]} EXCEPT ALL '
                f'SELECT * FROM {WRITE_TABLES["gin"]}) UNION ALL (SELECT * FROM {WRITE_TABLES["gin"]} '
                f'EXCEPT ALL SELECT * FROM {WRITE_TABLES["pin"]})) AS difference')
    if cur.fetchone()[0] != 0:
        raise RuntimeError('write/maintenance fixture identities differ')


def concurrent(args) -> None:
    if not args.concurrent_seconds:
        return
    # one acknowledged write per sample; an older repeatable-read snapshot retains exact identities.
    completed, resume, stop = threading.Event(), threading.Event(), threading.Event()
    errors, events = [], []

    def writer() -> None:
        try:
            with closing(connection(anchors=args.frontier_anchors)) as conn, conn.cursor() as cur:
                index = 0
                while not stop.is_set():
                    body = ('alpha beta rareplanet' if args.concurrent_related
                            else 'unrelated concurrent filler')
                    events.append(measured(cur, f"INSERT INTO {TABLE} VALUES "
                                           f"({10_000_000 + index}, {literal(body)})"))
                    index += 1
                    completed.set()
                    if not resume.wait(120):
                        raise TimeoutError('reader did not acknowledge writer progress')
                    resume.clear()
        except BaseException as error:
            errors.append(repr(error))
            completed.set()

    thread = threading.Thread(target=writer, daemon=True)
    results = []
    with closing(connection(anchors=args.frontier_anchors)) as conn, conn.cursor() as cur:
        cur.execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY')
        verify(cur, args.cases)
        thread.start()
        deadline = time.monotonic() + args.concurrent_seconds
        try:
            while time.monotonic() < deadline:
                if not completed.wait(120) or errors:
                    raise RuntimeError(f'concurrent writer failed: {errors}')
                completed.clear()
                # the next write overlaps the read pass; separate progress records prevent hidden starvation.
                resume.set()
                for mode in MODES:
                    cur.execute('SET pin.enable_grouped_scan = ' + ('on' if mode == MODES[0] else 'off'))
                    for case in args.cases:
                        record = measured(cur, f'SELECT count(*) FROM ONLY {TABLE} WHERE {predicate(case, mode)}')
                        results.append({'case': case, 'mode': mode, **record})
                verify(cur, args.cases)
        finally:
            stop.set()
            resume.set()
            thread.join(130)
            cur.execute('ROLLBACK')
            save(args.output / 'concurrent.json', {'reads': results, 'writes': events, 'errors': errors,
                 'measurement': 'one long repeatable-read reader, acknowledged concurrent inserts',
                 'related_writes': args.concurrent_related})
        if thread.is_alive() or errors:
            raise RuntimeError(f'concurrent qualification incomplete: {errors}')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--bindir', type=Path, required=True)
    parser.add_argument('--revision', required=True)
    parser.add_argument('--disposable', action='store_true', required=True)
    parser.add_argument('--frontier-anchors', action='store_true')
    parser.add_argument('--rows', type=int, default=20000)
    parser.add_argument('--deltas', type=int, nargs='+', default=[0, 1, 1000, 10000])
    parser.add_argument('--related-rows', type=int, default=2048)
    parser.add_argument('--queries', type=int, default=20)
    parser.add_argument('--samples', type=int, default=3)
    parser.add_argument('--seconds', type=float, default=2)
    parser.add_argument('--profile-seconds', type=float, default=0)
    parser.add_argument('--throughput-seconds', type=int, default=0)
    parser.add_argument('--write-batches', type=int, default=3)
    parser.add_argument('--write-rows', type=int, default=1000)
    parser.add_argument('--write-update-rows', type=int, default=0)
    parser.add_argument('--concurrent-related', action='store_true')
    parser.add_argument('--concurrent-seconds', type=float, default=0)
    parser.add_argument('--cases', nargs='+', choices=CASES, default=list(CASES))
    args = parser.parse_args()
    try:
        validate_deltas(args.deltas)
        if len(set(args.cases)) != len(args.cases):
            raise ValueError('each query class may be selected only once')
        if not (1024 <= args.rows <= 2_000_000 and 1 <= args.related_rows <= 1_000_000
                and 1 <= args.queries <= 1000 and 1 <= args.samples <= 20
                and 0.5 <= args.seconds <= 120 and 0 <= args.profile_seconds <= 120
                and 1 <= args.write_batches <= 20 and 1 <= args.write_rows <= 100000
                and 0 <= args.write_update_rows <= args.write_rows
                and 0 <= args.concurrent_seconds <= 120 and 0 <= args.throughput_seconds <= 120):
            raise ValueError('argument outside bounded qualification range')
    except ValueError as error:
        parser.error(str(error))
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    try:
        with closing(connection(anchors=args.frontier_anchors)) as conn, conn.cursor() as cur:
            cur.execute("SELECT json_object_agg(name, setting) FROM pg_settings WHERE name IN "
                        "('server_version_num','block_size','fsync','full_page_writes','synchronous_commit',"
                        "'shared_buffers','work_mem','maintenance_work_mem','autovacuum','pin.enable_frontier_anchors')")
            settings = cur.fetchone()[0]
            cur.execute('SELECT pin.build_revision(), current_setting(\'data_directory\'), pg_backend_pid()')
            revision, data, pid = cur.fetchone()
            cur.execute('SELECT inet_server_addr() IS NULL')
            if not cur.fetchone()[0]:
                raise RuntimeError('a local unix-domain server connection is required')
            validate_host(settings, revision, args.revision, Path(data))
            proc(pid)
            for table in [TABLE, *WRITE_TABLES.values()]:
                cur.execute('SELECT to_regclass(%s)', (table,))
                if cur.fetchone()[0] is not None:
                    raise RuntimeError(f'refusing to replace existing fixture {table}')
            save(args.output / 'environment.json', {'revision': revision, 'settings': settings,
                 'platform': platform.platform(), 'cpu_count': os.cpu_count(), 'data_directory': data,
                 'arguments': {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
                 'metric_caveats': ['backend cpu is not whole-cluster cpu', 'wal differences are cluster-wide',
                                    'warm serial reads are not production throughput', 'no ranked retrieval measurement',
                                    'concurrent writes update a dual-index table and are not attributable to one engine',
                                    'concurrent reads retain a pre-write snapshot; fresh visibility is checked after the stage']})
            cur.execute(f'CREATE TABLE {TABLE}(id bigint PRIMARY KEY, body text NOT NULL) '
                        'WITH (autovacuum_enabled = false)')
            cur.execute(f"INSERT INTO {TABLE} SELECT i, repeat('alpha beta gamma ', 32) || "
                        f"CASE WHEN i <= 20 THEN 'rareplanet' ELSE 'filler' END FROM generate_series(1, {args.rows}) AS i")
            cur.execute('SET pin.enable_grouped_storage = on')
            cur.execute(f'CREATE INDEX pin_g6_bench_body ON {TABLE} USING pin(body)')
            cur.execute(f"CREATE INDEX pin_g6_bench_body_gin ON {TABLE} USING gin(to_tsvector('simple', body))")
            cur.execute(f'VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0) {TABLE}')
            cur.execute('SET pin.enable_grouped_storage = off')
            previous = 0
            for size in args.deltas:
                if size != previous:
                    cur.execute(f"INSERT INTO {TABLE} SELECT i, 'unrelated filler' "
                                f"FROM generate_series({args.rows + previous + 1}, {args.rows + size}) AS i")
                read_stage(args, f'unrelated-{size}')
                previous = size
            first = args.rows + previous + 1
            cur.execute(f"INSERT INTO {TABLE} SELECT i, 'alpha beta rareplanet newterm' "
                        f"FROM generate_series({first}, {first + args.related_rows - 1}) AS i")
            read_stage(args, 'related-multipage')
            concurrent(args)
            verify(cur, args.cases)
            cur.execute('SET pin.enable_grouped_storage = on')
            save(args.output / 'snapshot-maintenance.json', measured(cur, f'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) {TABLE}'))
            cur.execute('SET pin.enable_grouped_storage = off')
            read_stage(args, 'rebuilt')
            writes(args, cur)
        save(args.output / 'status.json', {'status': 'completed', 'performance_targets_qualified': False})
    except BaseException as error:
        save(args.output / 'status.json', {'status': 'failed', 'error': repr(error), 'performance_targets_qualified': False})
        raise
    finally:
        files = {}
        for path in sorted(args.output.rglob('*')):
            if path.is_file() and path.name != 'manifest.json':
                with path.open('rb') as source:
                    files[str(path.relative_to(args.output))] = hashlib.file_digest(source, 'sha256').hexdigest()
        save(args.output / 'manifest.json', files)


if __name__ == '__main__':
    main()
