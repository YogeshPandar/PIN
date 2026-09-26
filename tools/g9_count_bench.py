#!/usr/bin/env python3
"""Fail-closed same-backend grouped COUNT/GIN experiment on a disposable database.

No results are supplied by this script. Run on the database host with a matching
PID namespace. Every sample includes SQL executor work; EXPLAIN is separate.
The corpus uses lowercase ASCII words and spaces, then verifies full identities
against PostgreSQL's simple dictionary before measuring any query class.
"""
from __future__ import annotations

import argparse
from collections import defaultdict
from contextlib import ExitStack
import hashlib
import filecmp
import itertools
import json
from pathlib import Path
import platform
import re
import statistics
import subprocess
import time

try:
    from .g9_profile import Session, literal, parse_proc_stat, percentiles, save
except ImportError:
    from g9_profile import Session, literal, parse_proc_stat, percentiles, save

MODES = ('pin_previous', 'pin_grouped', 'gin')
# the last two cases intentionally remain ordinary heap/executor workloads.
CASES = {
    'rare_count': ('rareplanet', 'rareplanet', False),
    'selective_and_count': ('alpha AND uncommon', 'alpha & uncommon', False),
    'broad_and_count': ('alpha AND bravo', 'alpha & bravo', False),
    'broad_or_count': ('alpha OR bravo', 'alpha | bravo', False),
    'not_count': ('alpha AND NOT uncommon', 'alpha & !uncommon', False),
    'exact_count': ('alpha', 'alpha', False),
    'phrase_count': ('"bravo charlie"', 'bravo <-> charlie', False),
    'broad_rows': ('alpha OR bravo', 'alpha | bravo', True),
    'ranked_topk': ('alpha OR bravo', 'alpha | bravo', True),
}
SCHEMA = 'pin_grouped_count_bench'
TABLE = SCHEMA + '.documents'


def orders(blocks: int) -> list[tuple[str, ...]]:
    if blocks < 6 or blocks > 60 or blocks % 6:
        raise ValueError('blocks must be a multiple of six in 6..60')
    return list(itertools.islice(itertools.cycle(itertools.permutations(MODES)), blocks))


def body_sql(key: str = 'g', rare: bool = True) -> str:
    if key not in ('g', 'id'):
        raise ValueError('trusted generator identifier required')
    # variable length and skew; 4,093 vocabulary buckets, without punctuation.
    rare_clause = f"CASE WHEN {key} % 65537 = 1 THEN ' rareplanet' ELSE '' END" if rare else "''"
    return (f"concat(CASE WHEN {key}%4<3 THEN 'alpha ' ELSE '' END,"
            f"CASE WHEN {key}%3<2 THEN 'bravo charlie ' ELSE 'charlie bravo ' END,"
            f"CASE WHEN {key}%97=0 THEN 'uncommon ' ELSE '' END,"
            f"translate(md5(({key}%4093)::text),'0123456789','ghijklmnop'),' ',"
            f"repeat('delta echo ',1+({key}%13)::int), {rare_clause})")


def predicate(case: str, mode: str) -> str:
    pin, gin, _ = CASES[case]
    return (f"to_tsvector('simple',body) @@ to_tsquery('simple',{literal(gin)})"
            if mode in ('gin', 'oracle') else
            f'body OPERATOR(pin.@@@) pin.parse_query({literal(pin)})')


def sql_for(case: str, mode: str) -> str:
    where = predicate(case, mode)
    if case == 'ranked_topk':
        # identical expression, normalization, source text and deterministic tie order.
        score = f"ts_rank_cd(to_tsvector('simple',body),to_tsquery('simple',{literal(CASES[case][1])}),0)"
        return f'SELECT id,{score} AS score FROM ONLY {TABLE} WHERE {where} ORDER BY score DESC,id LIMIT 20;'
    if case == 'broad_rows':
        return f'SELECT id FROM ONLY {TABLE} WHERE {where} ORDER BY id;'
    return f'SELECT count(*) FROM ONLY {TABLE} WHERE {where};'


def settings(session: Session, mode: str) -> None:
    session.execute('SET pin.enable_grouped_scan=on; SET pin.enable_exact_bitmap=on; '
                    'SET pin.enable_count_vm=on; SET pin.parallel_count_workers=0; '
                    'SET pin.enable_count_fastpath=' + ('off' if mode in ('gin', 'oracle') else 'on') + '; '
                    'SET pin.enable_grouped_count=' + ('on' if mode == 'pin_grouped' else 'off') + '; '
                    'SET enable_seqscan=' + ('on' if mode == 'oracle' else 'off') + '; '
                    'SET enable_bitmapscan=' + ('off' if mode == 'oracle' else 'on') + ';')


def nodes(root: dict):
    yield root
    for child in root.get('Plans', []):
        yield from nodes(child)


def check_plan(plan: list, mode: str, case: str, *, executed: bool, require_grouped: bool = False) -> dict:
    if len(plan) != 1:
        raise ValueError('one plan required')
    all_nodes = list(nodes(plan[0]['Plan']))
    custom = [n for n in all_nodes if n.get('Custom Plan Provider') == 'PinCount']
    index_name = 'documents_gin' if mode == 'gin' else 'documents_pin'
    indexes = [n for n in all_nodes if n.get('Node Type') == 'Bitmap Index Scan']
    if len(indexes) != 1 or indexes[0].get('Index Name') != index_name:
        raise ValueError(f'{mode}/{case}: expected retained bitmap plan on {index_name}')
    if any(n.get('Node Type') in ('Gather', 'Gather Merge', 'Seq Scan') for n in all_nodes):
        raise ValueError('serial forced-index experiment selected a different plan')
    if mode == 'gin' and custom:
        raise ValueError('GIN control must not execute a PIN custom aggregate')
    if case in ('phrase_count', 'broad_rows', 'ranked_topk') and custom:
        raise ValueError('unsupported shape entered custom aggregate')
    grouped = sum(n.get('Grouped Count Runs', 0) for n in custom)
    if require_grouped and executed and mode == 'pin_grouped' and case not in ('phrase_count', 'broad_rows', 'ranked_topk'):
        if not grouped:
            raise ValueError('grouped path was not executed; archive fallback, do not report as a win')
    return {'grouped_runs': grouped, 'custom': custom,
            'execution_path': ('grouped' if grouped else 'fallback' if any('Fallback' in n for n in custom)
                               else 'previous_count' if custom else 'ordinary_postgres'),
            'execution_ms': plan[0].get('Execution Time'),
            'root_inclusive': {k: v for k, v in plan[0]['Plan'].items()
                               if k.endswith('Blocks') or k in ('Actual Rows', 'Actual Loops')}}


def cpu_read(pid: int) -> dict:
    root = Path('/proc') / str(pid)
    stat = parse_proc_stat((root / 'stat').read_text(), pid)
    fields = [int(x) for x in (root / 'schedstat').read_text().split()]
    if len(fields) != 3 or any(x < 0 for x in fields):
        raise ValueError('unexpected Linux schedstat')
    return {'pid': pid, 'start': stat.start, 'cpu_ns': fields[0], 'runqueue_ns': fields[1]}


def cpu_delta(before: dict, after: dict, count: int) -> dict:
    if count <= 0 or (before['pid'], before['start']) != (after['pid'], after['start']):
        raise ValueError('invalid denominator or backend PID reuse')
    delta = {k: after[k] - before[k] for k in ('cpu_ns', 'runqueue_ns')}
    if min(delta.values()) < 0 or delta['cpu_ns'] == 0:
        raise ValueError('invalid or unavailable schedstat measurement')
    return {**delta, 'cpu_us_per_query': delta['cpu_ns'] / count / 1000,
            'short_batch_warning': delta['cpu_ns'] < 100_000_000}



def run_result(session: Session, case: str, mode: str, destination: Path | None = None) -> str:
    if case != 'broad_rows':
        return session.execute(f'EXECUTE measured_{mode};')
    # psql still receives every row. Redirect only client formatting/output, never
    # replace the SQL by a count or omit heap work. All modes use the same sink.
    target = str(destination.resolve()) if destination is not None else '/dev/null'
    if re.fullmatch(r'[A-Za-z0-9_./-]+', target) is None:
        raise ValueError('row artifacts require a path without shell or psql metacharacters')
    session.execute(f"\\o {target}\nEXECUTE measured_{mode};\n\\o\n;")
    return 'rows delivered to psql; identity stream checked outside timing'


def sha256(path: Path) -> str:
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def identities(session: Session, case: str) -> tuple[str, int]:
    # full ordered identities compared batch by batch, not just a count or hash.
    session.execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
    try:
        for mode in ('oracle', 'pin_previous', 'gin'):
            settings(session, mode)
            session.execute(f'DECLARE ids_{mode} NO SCROLL CURSOR FOR SELECT id,ctid FROM ONLY '
                            f'{TABLE} WHERE {predicate(case, mode)} ORDER BY id,ctid;')
        digest = hashlib.sha256()
        count = 0
        while True:
            batches = [session.execute(f'FETCH FORWARD 512 FROM ids_{mode};')
                       for mode in ('oracle', 'pin_previous', 'gin')]
            if len(set(batches)) != 1:
                raise AssertionError(f'{case}: full row identities differ')
            rows = batches[0].splitlines()
            count += len(rows)
            digest.update((batches[0] + '\n').encode())
            if len(rows) < 512:
                break
        return digest.hexdigest(), count
    finally:
        session.execute('ROLLBACK;')


def ddl_measure(session: Session, pid: int, statement: str) -> dict:
    lsn = session.execute('SELECT pg_current_wal_insert_lsn();')
    before = cpu_read(pid)
    start = time.perf_counter_ns()
    session.execute(statement)
    wall = time.perf_counter_ns() - start
    after = cpu_read(pid)
    wal = int(session.execute(f'SELECT pg_wal_lsn_diff(pg_current_wal_insert_lsn(),{literal(lsn)})::bigint;'))
    return {'backend_cpu': cpu_delta(before, after, 1), 'wall_ns': wall,
            'cluster_wal_bytes': wal, 'sql': statement}


def writes(session: Session, pid: int, rows: int, first: str) -> list[dict]:
    result = []
    order = ('pin', 'gin') if first == 'pin' else ('gin', 'pin')
    for engine in order:
        table = SCHEMA + '.write_' + engine
        session.execute(f'CREATE TABLE {table}(id bigint,body text) WITH (autovacuum_enabled=false);')
        target = 'body' if engine == 'pin' else "to_tsvector('simple',body)"
        session.execute(f'CREATE INDEX ON {table} USING {engine}({target});')
        insert = ddl_measure(session, pid, f'INSERT INTO {table} SELECT g,{body_sql()} FROM generate_series(1,{rows}) g;')
        vacuum = ddl_measure(session, pid, f'VACUUM (INDEX_CLEANUP ON,PARALLEL 0) {table};')
        size = int(session.execute(f"SELECT pg_indexes_size({literal(table)});") )
        result.append({'engine': engine, 'rows': rows, 'insert': insert, 'vacuum': vacuum,
                       'index_bytes': size, 'one_search_index_only': True})
    return result


def measure(args: argparse.Namespace) -> None:
    orders(args.blocks)
    if not args.same_host:
        raise ValueError('--same-host asserts PostgreSQL and this process share a PID namespace')
    output: Path = args.output
    with ExitStack() as stack:
        session = stack.enter_context(Session(args.bindir / 'psql', output / 'backend.stderr',
                                               'pin_grouped_enabled', timeout=3600,
                                               frontier_anchors=args.anchors,
                                               owner_frontier=args.owner_frontier))
        session.execute("SET statement_timeout='3500s'; SET lock_timeout='30s';")
        pid = int(session.execute('SELECT pg_backend_pid();'))
        environment = json.loads(session.execute('SELECT json_object_agg(name,setting) FROM pg_settings;'))
        if environment.get('server_version_num') != '180006' or environment.get('block_size') != '8192':
            raise ValueError('requires pinned PostgreSQL 18.6 and 8 KiB pages')
        if any(environment.get(k) != 'on' for k in ('fsync', 'full_page_writes', 'synchronous_commit')):
            raise ValueError('equal durable configuration required')
        if session.execute('SELECT pg_is_in_recovery();') != 'f':
            raise ValueError('standby search is not supported')
        required_gucs = ('pin.enable_grouped_count', 'pin.enable_grouped_scan',
                         'pin.enable_grouped_storage', 'pin.enable_count_vm',
                         'pin.enable_count_fastpath', 'pin.enable_frontier_anchors',
                         'pin.enable_owner_frontier')
        if any(name not in environment for name in required_gucs):
            raise ValueError('required GUCs are not registered by this loaded binary')
        revision = session.execute('SELECT pin.build_revision();')
        if revision != args.expected_revision:
            raise ValueError('binary build revision differs from the requested source revision')
        cpu_read(pid)
        source_root = Path(__file__).resolve().parents[1]
        source_paths = [source_root / 'Cargo.toml', source_root / 'Cargo.lock',
                        source_root / 'rust-toolchain.toml']
        source_paths += sorted((source_root / 'crates').rglob('*.rs'))
        source_paths += sorted((source_root / 'crates').rglob('*.c'))
        source_paths += sorted((source_root / 'crates').rglob('*.h'))
        source_paths += sorted((source_root / 'tools').glob('g9_count_*'))
        save(output / 'source-SHA256.json', {str(p.relative_to(source_root)): sha256(p)
             for p in source_paths if p.is_file()})
        for name, command in (('source-head.txt', ['rev-parse', 'HEAD']),
                              ('source-status.txt', ['status', '--porcelain=v1']),
                              ('source-diff.patch', ['diff', '--binary', 'HEAD'])):
            capture = subprocess.run(['git', '-C', str(source_root), *command],
                                     capture_output=True, text=True, timeout=30, check=False)
            (output / name).write_text(capture.stdout + capture.stderr)
            if capture.returncode:
                raise ValueError('source provenance requires the actual git worktree')
        if (output / 'source-head.txt').read_text().strip() != revision:
            raise ValueError('harness worktree and binary revisions differ')
        if (output / 'source-status.txt').read_text().strip():
            raise ValueError('commit tracked and untracked changes before a benchmark claim')
        save(output / 'environment.json', {'server_settings': environment, 'pid': pid,
             'revision': revision, 'pin_so_sha256': sha256(args.pin_so),
             'postgres_sha256': sha256(args.bindir / 'postgres'),
             'host_note': args.host_note, 'platform': platform.platform(),
             'cpuinfo': Path('/proc/cpuinfo').read_text(), 'cli': vars(args) | {
                 'bindir': str(args.bindir), 'pin_so': str(args.pin_so), 'output': str(output)},
             'not_measured': ['cold-cache', 'hardware PMU', 'concurrent writer p95',
                              'natural-language production corpus', 'TIN'],
             'latency_scope': 'client roundtrip through persistent psql; not pure executor latency',
             'cpu_scope': 'one PostgreSQL backend schedstat; excludes other processes'})
        # never silently drop user data; a new dedicated schema is mandatory.
        session.execute(f'CREATE SCHEMA {SCHEMA}; SET pin.enable_grouped_storage=on; '
                        'SET pin.enable_grouped_delta_seal=' + ('on' if args.delta_seal else 'off') + '; '
                        'SET pin.enable_grouped_page_visibility=' + ('on' if args.page_visibility else 'off') + ';')
        samples = []
        with (output / 'samples.jsonl').open('x') as journal:
            for corpus, rows in enumerate(args.rows):
                if corpus:
                    session.execute(f'DROP TABLE {TABLE};')
                session.execute(f'CREATE TABLE {TABLE}(id bigint,body text,notes integer DEFAULT 0) '
                                'WITH (fillfactor=80,autovacuum_enabled=false);')
                session.execute(f'INSERT INTO {TABLE}(id,body) SELECT g,{body_sql()} FROM generate_series(1,{rows}) g;')
                build = []
                for engine in (('pin', 'gin') if corpus % 2 == 0 else ('gin', 'pin')):
                    target = 'body' if engine == 'pin' else "to_tsvector('simple',body)"
                    build.append({'engine': engine, **ddl_measure(session, pid,
                        f'CREATE INDEX documents_{engine} ON {TABLE} USING {engine}({target});')})
                session.execute(f'VACUUM (ANALYZE,INDEX_CLEANUP ON,PARALLEL 0) {TABLE};')
                save(output / f'{rows}-build.json', build)
                stage_sql = {
                    'fresh': None,
                    'short_delta': f'INSERT INTO {TABLE}(id,body) SELECT g,{body_sql(rare=False)} FROM generate_series({rows+1},{rows+17}) g;',
                    'long_delta': f'INSERT INTO {TABLE}(id,body) SELECT g,{body_sql(rare=False)} FROM generate_series({rows+18},{rows+4096}) g;',
                    'updated_deleted': f"UPDATE {TABLE} SET notes=notes+1 WHERE id%101=0; UPDATE {TABLE} SET body='bravo charlie' WHERE id%103=0; DELETE FROM {TABLE} WHERE id%107=0;",
                    'rebuilt': f'VACUUM (ANALYZE,INDEX_CLEANUP ON,PARALLEL 0) {TABLE};',
                }
                for stage, mutation in stage_sql.items():
                    if mutation:
                        session.execute(mutation)
                    folder = output / f'{rows}-{stage}'
                    folder.mkdir()
                    save(folder / 'sizes.json', json.loads(session.execute(
                        f"SELECT json_build_object('heap',pg_relation_size('{TABLE}'),"
                        f"'pin',pg_relation_size('{SCHEMA}.documents_pin'),"
                        f"'gin',pg_relation_size('{SCHEMA}.documents_gin'));")))
                    for case in args.cases:
                        digest, cardinality = identities(session, case)
                        save(folder / f'{case}-oracle.json', {'ordered_id_ctid_sha256': digest,
                             'rows': cardinality, 'full_rows_compared': True})
                        session.execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
                        expected = None
                        try:
                            probes = {}
                            for mode in MODES:
                                settings(session, mode)
                                sql = sql_for(case, mode)
                                (folder / f'{case}-{mode}.sql').write_text(sql + '\n')
                                session.execute(f'PREPARE measured_{mode} AS {sql}')
                                got = run_result(session, case, mode, folder / f'{case}-{mode}-rows.txt')
                                if case == 'broad_rows' and mode != MODES[0]:
                                    if not filecmp.cmp(folder / f'{case}-{MODES[0]}-rows.txt',
                                                       folder / f'{case}-{mode}-rows.txt', shallow=False):
                                        raise AssertionError('full delivered ordered rows differ')
                                if expected is None:
                                    expected = got
                                if got != expected or (not CASES[case][2] and got != str(cardinality)):
                                    raise AssertionError('count, row sequence or exact PG rank differs')
                                plan = json.loads(session.execute('EXPLAIN (ANALYZE,BUFFERS,WAL,SETTINGS,TIMING OFF,FORMAT JSON) '
                                                                 f'EXECUTE measured_{mode};'))
                                save(folder / f'{case}-{mode}-plan.json', plan)
                                probes[mode] = check_plan(plan, mode, case, executed=True)
                            for block, order in enumerate(orders(args.blocks)):
                                for position, mode in enumerate(order):
                                    settings(session, mode)
                                    for _ in range(args.warmup):
                                        if run_result(session, case, mode) != expected:
                                            raise AssertionError('warmup result changed')
                                    before = cpu_read(pid)
                                    latencies = []
                                    for _ in range(args.queries):
                                        start = time.perf_counter_ns()
                                        got = run_result(session, case, mode)
                                        latencies.append((time.perf_counter_ns()-start)/1e6)
                                        if got != expected:
                                            raise AssertionError('measured result changed')
                                    after = cpu_read(pid)
                                    record = {'rows': rows, 'stage': stage, 'case': case, 'mode': mode,
                                              'block': block, 'position': position, 'order': order,
                                              'latency_ms': latencies, 'client_latency': percentiles(latencies),
                                              'throughput_qps': len(latencies)*1000/sum(latencies),
                                              'backend_cpu': cpu_delta(before, after, args.queries),
                                              'separate_explain_probe': probes[mode]}
                                    journal.write(json.dumps(record, allow_nan=False)+'\n'); journal.flush()
                                    samples.append(record)
                            for mode in MODES:
                                session.execute(f'DEALLOCATE measured_{mode};')
                        finally:
                            session.execute('ROLLBACK;')
                print(f'completed {rows} rows', flush=True)
        save(output / 'write-maintenance.json', writes(session, pid, args.write_rows,
                                                       'pin' if len(args.rows) % 2 == 0 else 'gin'))
        grouped = defaultdict(list)
        for sample in samples:
            grouped[(sample['rows'], sample['stage'], sample['case'], sample['mode'])].append(sample)
        summary = []
        for key, group in sorted(grouped.items()):
            values = [x['backend_cpu']['cpu_us_per_query'] for x in group]
            latency = list(itertools.chain.from_iterable(x['latency_ms'] for x in group))
            summary.append({'rows': key[0], 'stage': key[1], 'case': key[2], 'mode': key[3],
                            'median_batch_cpu_us': statistics.median(values),
                            'batch_cpu_us': values, 'client_latency': percentiles(latency),
                            'execution_paths': sorted({x['separate_explain_probe']['execution_path'] for x in group}),
                            'tail_sample_warning': len(latency) < 1000})
        save(output / 'summary.json', summary)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bindir', type=Path, required=True)
    parser.add_argument('--pin-so', type=Path, required=True)
    parser.add_argument('--expected-revision', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--host-note', required=True)
    parser.add_argument('--same-host', action='store_true')
    parser.add_argument('--rows', nargs='+', type=int, default=[20_000, 1_000_000])
    parser.add_argument('--blocks', type=int, default=6)
    parser.add_argument('--queries', type=int, default=100)
    parser.add_argument('--warmup', type=int, default=5)
    parser.add_argument('--write-rows', type=int, default=1000)
    parser.add_argument('--cases', nargs='+', choices=tuple(CASES), default=list(CASES))
    parser.add_argument('--anchors', action='store_true')
    parser.add_argument('--owner-frontier', action='store_true')
    parser.add_argument('--delta-seal', action='store_true')
    parser.add_argument('--page-visibility', action='store_true')
    args = parser.parse_args()
    if (any(n < 1200 or n > 10_000_000 for n in args.rows) or len(set(args.rows)) != len(args.rows)
            or not 1 <= args.queries <= 10000 or not 0 <= args.warmup <= 100
            or not 1 <= args.write_rows <= 1_000_000 or not re.fullmatch('[0-9a-f]{40}', args.expected_revision)):
        parser.error('invalid corpus, sample, write or revision bounds')
    args.output.mkdir(parents=True, exist_ok=False)
    try:
        measure(args)
    except BaseException as error:
        save(args.output/'status.json', {'status': 'failed', 'error': repr(error)})
        raise
    else:
        save(args.output/'status.json', {'status': 'complete', 'performance_claim': 'requires review of raw controls'})
    finally:
        save(args.output/'SHA256.json', {str(p.relative_to(args.output)): sha256(p)
             for p in sorted(args.output.rglob('*')) if p.is_file() and p.name != 'SHA256.json'})


if __name__ == '__main__':
    main()
