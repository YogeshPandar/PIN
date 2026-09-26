#!/usr/bin/env python3
"""Capture backend syscall totals for warm ordinary phrase bitmap queries."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from g9_count_bench import TABLE, predicate
from g9_profile import Session


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--queries', type=int, default=40)
    parser.add_argument('--positions', action='store_true')
    args = parser.parse_args()
    if args.queries < 2:
        parser.error('at least two queries required')
    args.output.mkdir(parents=True, exist_ok=False)
    with Session(Path('/usr/lib/postgresql/18/bin/psql'), args.output / 'psql.stderr',
                 'pin_legacy') as session:
        pid = int(session.execute('SELECT pg_backend_pid();'))
        revision = session.execute('SELECT pin.build_revision();')
        session.execute('SET pin.enable_grouped_scan=on; SET pin.enable_exact_bitmap=on; '
                        'SET pin.enable_count_fastpath=off; SET pin.enable_grouped_count=off; '
                        'SET enable_seqscan=off; SET enable_indexscan=off; SET jit=off; '
                        'SET pin.enable_phrase_positions=' + ('on' if args.positions else 'off') + ';')
        statement = (f'SELECT count(*) FROM ONLY {TABLE} WHERE '
                     f'{predicate("phrase_count", "pin_previous")};')
        expected = session.execute(statement)
        trace_path = args.output / 'strace.txt'
        stderr = (args.output / 'strace.stderr').open('wb')
        trace = subprocess.Popen(['sudo', '-n', 'strace', '-f', '-c', '-p', str(pid),
                                  '-o', str(trace_path)], stdout=subprocess.DEVNULL, stderr=stderr)
        try:
            time.sleep(.3)
            if trace.poll() is not None:
                raise RuntimeError('strace did not attach')
            start = time.monotonic()
            for _ in range(args.queries):
                if session.execute(statement) != expected:
                    raise AssertionError('query result changed')
            elapsed = time.monotonic() - start
        finally:
            trace.send_signal(signal.SIGINT)
            trace.wait(timeout=15)
            stderr.close()
    (args.output / 'metadata.json').write_text(json.dumps({
        'revision': revision, 'pid': pid, 'queries': args.queries,
        'phrase_positions': args.positions, 'answer': expected,
        'client_elapsed_seconds_with_strace': elapsed,
        'trace_exit': trace.returncode,
    }, indent=2) + '\n')
    print(trace_path.read_text())


if __name__ == '__main__':
    main()
