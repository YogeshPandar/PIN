#!/usr/bin/env python3
"""Exercise exact phrase bitmap results across PostgreSQL heap lifecycle changes."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import uuid

from g9_profile import Session


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--direct-documents', action='store_true')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    schema = 'pin_phrase_' + uuid.uuid4().hex[:12]
    table = schema + '.docs'
    record = []
    with Session(Path('/usr/lib/postgresql/18/bin/psql'), args.output / 'psql.stderr',
                 'pin_legacy') as session:
        if args.direct_documents:
            session.execute('SET pin.enable_direct_documents=on;')
        session.execute(f'CREATE SCHEMA {schema};')
        try:
            session.execute(f'CREATE TABLE {table}(id int PRIMARY KEY, body text, note int DEFAULT 0) '
                            'WITH (fillfactor=70,autovacuum_enabled=false);')
            session.execute(f"INSERT INTO {table}(id,body) VALUES "
                            "(1,'alpha beta gamma'),(2,'beta alpha gamma'),"
                            "(3,'alpha beta'),(4,'echo echo delta'),"
                            "(5,'Éclair café'),(6,repeat('echo ',10000)),(99,repeat('middle ',12000)||'zulu omega');")
            session.execute('SET pin.enable_grouped_storage=on; SET pin.enable_grouped_delta_seal=on;')
            session.execute(f'CREATE INDEX docs_pin ON {table} USING pin(body);')
            session.execute(f"CREATE INDEX docs_gin ON {table} USING gin(to_tsvector('simple',body));")
            session.execute(f'VACUUM (ANALYZE,INDEX_CLEANUP ON,PARALLEL 0) {table};')
            session.execute('SET pin.enable_count_fastpath=off; SET pin.enable_grouped_count=off; '
                            'SET pin.enable_grouped_scan=on; SET pin.enable_exact_bitmap=on; '
                            'SET enable_indexscan=off; SET enable_seqscan=off; SET enable_bitmapscan=on;')

            def check(stage: str, active: Session | None = None) -> None:
                reader = active or session
                for source, gin in [('"alpha beta"', 'alpha <-> beta'),
                                    ('"echo echo"', 'echo <-> echo'),
                                    ('"éclair café"', 'éclair <-> café'),
                                    ('"zulu omega"', 'zulu <-> omega')]:
                    results = {}
                    for mode in ('positions', 'legacy', 'gin'):
                        reader.execute('SET pin.enable_phrase_positions=' +
                                       ('on' if mode == 'positions' else 'off') + ';')
                        clause = (f"to_tsvector('simple',body) @@ to_tsquery('simple','{gin}')"
                                  if mode == 'gin' else
                                  f"body OPERATOR(pin.@@@) pin.parse_query('{source}')")
                        results[mode] = reader.execute(
                            f'SELECT id,ctid FROM ONLY {table} WHERE {clause} ORDER BY id,ctid;')
                    if len(set(results.values())) != 1:
                        raise AssertionError(f'{stage}: {source}: {results}')
                    record.append({'stage': stage, 'phrase': source, 'rows': results['positions']})

            check('built')
            session.execute(f'UPDATE {table} SET note=note+1 WHERE id IN (1,4,6);')
            check('hot_update')
            session.execute(f"UPDATE {table} SET body='beta alpha' WHERE id=3;")
            check('indexed_update')
            session.execute('BEGIN;')
            session.execute(f"INSERT INTO {table}(id,body) VALUES (7,'alpha beta');")
            check('uncommitted_self_visible')
            session.execute('ROLLBACK;')
            check('aborted_insert')
            session.execute(f'DELETE FROM {table} WHERE id=1;')
            check('deleted')
            session.execute(f'VACUUM (ANALYZE,INDEX_CLEANUP ON,PARALLEL 0) {table};')
            check('vacuum')
            session.execute(f'REINDEX INDEX {schema}.docs_pin;')
            check('reindex')
            with Session(Path('/usr/lib/postgresql/18/bin/psql'),
                         args.output / 'snapshot.stderr', 'pin_legacy') as snapshot:
                snapshot.execute('SET pin.enable_count_fastpath=off; SET pin.enable_grouped_count=off; '
                                 'SET pin.enable_grouped_scan=on; SET pin.enable_exact_bitmap=on; '
                                 'SET enable_indexscan=off; SET enable_seqscan=off; '
                                 'SET enable_bitmapscan=on;')
                snapshot.execute('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;')
                prior = len(record)
                check('snapshot_before', snapshot)
                before = [item['rows'] for item in record[prior:]]
                session.execute(f"UPDATE {table} SET body='alpha beta' WHERE id=2;")
                session.execute(f"INSERT INTO {table}(id,body) VALUES (7,'alpha beta');")
                prior = len(record)
                check('snapshot_after_write', snapshot)
                if [item['rows'] for item in record[prior:]] != before:
                    raise AssertionError('repeatable-read snapshot changed after writer commit')
                snapshot.execute('ROLLBACK;')
            check('committed_after_snapshot')
        finally:
            session.execute(f'DROP SCHEMA {schema} CASCADE;')
    (args.output / 'result.json').write_text(json.dumps(record, indent=2) + '\n')
    print(f'{len(record)} phrase and stage identity comparisons passed')


if __name__ == '__main__':
    main()
