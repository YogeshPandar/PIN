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
    parser.add_argument('--direct-documents', action='store_true')
    parser.add_argument('--late-terms', action='store_true', help='place requested rare terms after the dense stream in lexical order')
    parser.add_argument('--rows', type=int, default=256)
    parser.add_argument('--tokens', type=int, default=10000)
    parser.add_argument('--profile-seconds', type=float, default=0, help='separate cpu-clock sampling duration per phrase')
    parser.add_argument('--pin-only', action='store_true', help='omit GIN for documents beyond its comparable positional range')
    parser.add_argument('--stored-control', action='store_true', help='time positions against GIN on a stored tsvector')
    parser.add_argument('--blocks', type=int, default=6)
    parser.add_argument('--queries', type=int, default=6)
    args = parser.parse_args()
    if min(args.rows, args.blocks, args.queries) < 2:
        parser.error('rows, blocks and queries must each be at least two')
    if not 0 <= args.profile_seconds <= 30:
        parser.error('profile-seconds must be between 0 and 30')
    if args.pin_only and args.stored_control:
        parser.error('pin-only and stored-control are mutually exclusive')
    limit = 260000 if args.pin_only else 16000
    if not 2 <= args.tokens <= limit:
        parser.error(f'tokens must be between 2 and {limit} for this mode')
    args.output.mkdir(parents=True, exist_ok=False)
    modes = ('positions',) if args.pin_only else (('positions', 'gin_stored') if args.stored_control else ('legacy', 'positions', 'gin'))
    table = 'public.pin_fragment_' + uuid.uuid4().hex[:12]
    samples, checks, plans, statements, profiles = [], [], {}, [], []
    with Session(Path('/usr/lib/postgresql/18/bin/psql'), args.output / 'psql.stderr', 'pin_legacy') as s:
        def execute(sql):
            statements.append(sql)
            return s.execute(sql.rstrip(';') + ';')
        if args.direct_documents:
            execute('SET pin.enable_direct_documents=on;')
        first, second = ('omega', 'zulu') if args.late_terms else ('alpha', 'beta')
        pid = int(execute('SELECT pg_backend_pid();'))
        revision = execute('SELECT pin.build_revision();')
        server = execute('SELECT version();')
        settings = execute('SHOW ALL;')
        (args.output / 'settings.txt').write_text(settings + '\n')
        execute(f'CREATE TABLE {table}(id int PRIMARY KEY, body text) WITH (autovacuum_enabled=false);')
        try:
            execute(f"INSERT INTO {table} SELECT i, repeat('echo ',{args.tokens}) || CASE WHEN i%2=0 "
                    f"THEN '{first} {second}' ELSE '{second} {first}' END FROM generate_series(1,{args.rows}) i;")
            if args.stored_control:
                execute(f"ALTER TABLE {table} ADD COLUMN search_vector tsvector GENERATED ALWAYS AS (to_tsvector('simple',body)) STORED;")
                execute(f'CREATE INDEX ON {table} USING gin(search_vector);')
            build_lsn = execute('SELECT pg_current_wal_insert_lsn();')
            build_before = cpu_read(pid)
            build_start = time.perf_counter_ns()
            execute(f'CREATE INDEX ON {table} USING pin(body);')
            build_elapsed = (time.perf_counter_ns() - build_start) / 1e6
            build_after = cpu_read(pid)
            build = dict(before=build_before, after=build_after, cpu=cpu_delta(build_before, build_after, 1),
                         elapsed_ms=build_elapsed, wal_bytes=int(execute(
                             f"SELECT pg_wal_lsn_diff(pg_current_wal_insert_lsn(),'{build_lsn}');")))
            if not args.pin_only:
                execute(f"CREATE INDEX ON {table} USING gin(to_tsvector('simple',body));")
            execute(f'VACUUM (ANALYZE, INDEX_CLEANUP ON, PARALLEL 0) {table};')
            (args.output / 'index_sizes.txt').write_text(execute(f"SELECT c.relname,a.amname,pg_relation_size(c.oid) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_am a ON a.oid=c.relam WHERE i.indrelid='{table}'::regclass ORDER BY c.relname;") + '\n')
            execute('SET pin.enable_count_fastpath=off; SET pin.enable_grouped_count=off;')
            execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
            for name, source, gin in [('adjacent', f'"{first} {second}"', f'{first} <-> {second}'),
                                      ('repeated', '"echo echo"', 'echo <-> echo'),
                                      ('nonadjacent', f'"{first} echo"', f'{first} <-> echo')]:
                def query(mode, projection='count(*)'):
                    clause = (f"to_tsvector('simple',body) @@ to_tsquery('simple','{gin}')" if mode == 'gin'
                              else f"search_vector @@ to_tsquery('simple','{gin}')" if mode == 'gin_stored'
                              else f"body OPERATOR(pin.@@@) pin.parse_query('{source}')")
                    return f'SELECT {projection} FROM ONLY {table} WHERE {clause}'
                def configure(mode):
                    execute('SET pin.enable_phrase_positions=' + ('on' if mode == 'positions' else 'off') + ';'
                            'SET enable_seqscan=' + ('on' if mode == 'oracle' else 'off') + ';'
                            'SET enable_bitmapscan=' + ('off' if mode == 'oracle' else 'on') + ';')
                identities = {}
                for mode in ('oracle', *modes):
                    configure(mode)
                    identities[mode] = execute(query(mode, 'id,ctid') + ' ORDER BY id,ctid;')
                    plans[name + '-' + mode] = json.loads(execute(
                        'EXPLAIN (ANALYZE,BUFFERS,TIMING OFF,FORMAT JSON) ' + query(mode)))
                    plan_text = json.dumps(plans[name + '-' + mode])
                    expected = 'Seq Scan' if mode == 'oracle' else 'Bitmap Index Scan'
                    if expected not in plan_text:
                        raise AssertionError(f'{name}/{mode}: missing {expected}')
                expected_filter = {'adjacent': 'id % 2 = 0', 'repeated': 'true', 'nonadjacent': 'false'}[name]
                identities['fixture_oracle'] = execute(f'SELECT id,ctid FROM {table} WHERE {expected_filter} ORDER BY id,ctid;')
                if len(set(identities.values())) != 1:
                    raise AssertionError(identities)
                checks.append({'case': name, 'identities': identities,
                               'sha256': hashlib.sha256(identities['oracle'].encode()).hexdigest()})
                for block in range(args.blocks):
                    order = modes if block % 2 == 0 else tuple(reversed(modes))
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
                if args.profile_seconds:
                    configure('positions')
                    sql = query('positions') + ';'
                    expected_answer = s.execute(sql)
                    data = args.output / (name + '.perf.data')
                    command = ['sudo', '-n', 'perf', 'record', '-e', 'cpu-clock', '-F', '199',
                               '-g', '--call-graph', 'dwarf,8192', '-p', str(pid), '-o', str(data),
                               '--', 'sleep', str(args.profile_seconds + 1)]
                    recorder = subprocess.Popen(command, stdout=subprocess.DEVNULL,
                                                stderr=subprocess.PIPE, text=True)
                    count = 0
                    try:
                        time.sleep(.25)
                        deadline = time.monotonic() + args.profile_seconds
                        while time.monotonic() < deadline:
                            if s.execute(sql) != expected_answer:
                                raise AssertionError('profile answer changed')
                            count += 1
                        stderr = recorder.communicate(timeout=15)[1]
                        (args.output / (name + '.perf.stderr')).write_text(stderr)
                        if recorder.returncode:
                            raise RuntimeError('perf record failed: ' + stderr)
                        report = subprocess.run(['sudo', '-n', 'perf', 'report', '--stdio', '--no-children',
                                                 '--sort', 'dso,symbol', '-i', str(data)],
                                                capture_output=True, text=True, check=True, timeout=60)
                        (args.output / (name + '.perf.txt')).write_text(report.stdout)
                        profiles.append(dict(case=name, queries=count, command=command, answer=expected_answer))
                    finally:
                        if recorder.poll() is None:
                            recorder.terminate()
                            recorder.wait(timeout=15)
            execute('ROLLBACK;')
        finally:
            execute('ROLLBACK;')
            execute(f'DROP TABLE {table};')
            (args.output / 'statements.sql').write_text('\n'.join(statements) + '\n')
    summary = []
    for name in ('adjacent', 'repeated', 'nonadjacent'):
        for mode in modes:
            selected = [x for x in samples if x['case'] == name and x['mode'] == mode]
            summary.append(dict(case=name, mode=mode, median_cpu_us=statistics.median(
                x['cpu']['cpu_us_per_query'] for x in selected)))
    (args.output / 'result.json').write_text(json.dumps(dict(
        revision=revision, profiles=profiles, pin_only=args.pin_only, server=server, platform=platform.platform(), build=build, direct_documents=args.direct_documents, late_terms=args.late_terms,
        cpu=subprocess.run(['lscpu'], capture_output=True, text=True, check=True).stdout, rows=args.rows, repeated_tokens=args.tokens, stored_control=args.stored_control, samples=samples, summary=summary, checks=checks), indent=2)+'\n')
    (args.output / 'plans.json').write_text(json.dumps(plans, indent=2)+'\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
