"""Validate the profiler protocol and attribution without claiming SQL speedups."""
from collections import Counter
from dataclasses import replace
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest

from tools import g9_profile as profile


class ProfileTests(unittest.TestCase):
    def test_complete_balanced_orders(self):
        orders = profile.orders(12)
        self.assertEqual(len(set(orders)), 6)
        for position in range(3):
            self.assertEqual(Counter(order[position] for order in orders),
                             dict.fromkeys(profile.MODES, 4))
        for count in (0, 1, 5, 7, 61):
            with self.assertRaises(ValueError):
                profile.orders(count)

    def test_plan_scopes_are_not_added_twice(self):
        plan = [{'Execution Time': 2.1, 'Plan': {
            'Node Type': 'Aggregate', 'Shared Hit Blocks': 12, 'Plans': [{
                'Node Type': 'Bitmap Heap Scan', 'Relation Name': 'pin_g6_bench',
                'Shared Hit Blocks': 12, 'Plans': [{
                    'Node Type': 'Bitmap Index Scan', 'Index Name': 'pin_g6_bench_body',
                    'Shared Hit Blocks': 2,
                }],
            }],
        }}]
        result = profile.inspect_plan(plan, 'pin_legacy')
        self.assertEqual(result['root_inclusive']['Shared Hit Blocks'], 12)
        self.assertEqual(result['index']['Shared Hit Blocks'], 2)
        with self.assertRaises(ValueError):
            profile.inspect_plan(plan, 'gin')
        index = plan[0]['Plan']['Plans'][0]['Plans'][0]
        plan[0]['Plan']['Plans'][0]['Plans'].append(index.copy())
        with self.assertRaises(ValueError):
            profile.inspect_plan(plan, 'pin_legacy')
        for kind in ('Custom Scan', 'Gather', 'Index Only Scan', 'Seq Scan'):
            with self.assertRaises(ValueError):
                profile.inspect_plan([{'Plan': {'Node Type': kind}}], 'pin_legacy')
        profile.inspect_plan([{'Plan': {'Node Type': 'Seq Scan'}}], 'oracle')
        with self.assertRaises(ValueError):
            profile.inspect_plan([{'Plan': {'Node Type': 'Custom Scan'}}], 'oracle')

    def test_backend_cpu_ticks_and_pid_reuse(self):
        fields = ['0'] * 22
        fields[0] = 'S'
        for index, value in ((19, 300), (11, 40), (12, 10), (7, 50), (9, 2), (21, 100)):
            fields[index] = str(value)
        text = '123 (postgres worker) ) ' + ' '.join(fields)
        before = profile.parse_proc_stat(text, 123)
        after = replace(before, user=140, system=20, minor=52)
        delta = profile.cpu_delta(before, after, 100, 10)
        self.assertEqual(delta['cpu_ms_per_query'], 110)
        self.assertEqual(delta['minor_faults'], 2)
        self.assertFalse(delta['resolution_warning'])
        self.assertTrue(profile.cpu_delta(before, before, 100, 10)['resolution_warning'])
        for invalid in (replace(after, start=301), replace(after, pid=124), replace(after, user=0)):
            with self.assertRaises(ValueError):
                profile.cpu_delta(before, invalid, 100, 10)
        for invalid in ('123 (postgres) S', text.replace('postgres', 'psql'), text):
            with self.assertRaises(ValueError):
                profile.parse_proc_stat(invalid, 124)
        self.assertIsNone(profile.read_cpu(None, 123)[0])
        with tempfile.TemporaryDirectory() as temp:
            value, error = profile.read_cpu(Path(temp), 123)
            self.assertIsNone(value)
            self.assertTrue(error)

    def test_percentiles_and_literals(self):
        result = profile.percentiles([1, 3, 2, 4])
        self.assertEqual((result['p50_ms'], result['p95_ms'], result['p99_ms']), (2, 4, 4))
        for values in ([], [float('nan')], [-1], [float('inf')]):
            with self.assertRaises(ValueError):
                profile.percentiles(values)
        self.assertEqual(profile.literal("a'b"), "'a''b'")
        for value in ('a\n', 'a\\', '\x00'):
            with self.assertRaises(ValueError):
                profile.literal(value)

    def test_same_count_different_ctids_is_rejected(self):
        class Fake:
            def __init__(self, rows):
                self.rows = rows
            def execute(self, sql):
                return self.rows if sql.startswith('FETCH') else ''
        sessions = {'oracle': Fake('(1,1)\n(1,2)'), 'gin': Fake('(1,1)\n(1,3)')}
        with self.assertRaises(ValueError):
            profile.matching_rows(sessions, 'common')
        sessions['gin'] = Fake('(1,1)\n(1,2)')
        self.assertEqual(profile.matching_rows(sessions, 'common'), 2)

    def test_persistent_pipe_chunks_errors_and_timeout(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            fake = root / 'psql'
            fake.write_text('#!' + sys.executable + '\n' + textwrap.dedent(r'''
                import os, sys, time
                count = 0
                for line in sys.stdin:
                    if line.startswith('FAIL'):
                        sys.exit(3)
                    if line.startswith('SLEEP'):
                        time.sleep(2)
                    if line.startswith('SELECT'):
                        count += 1
                        os.write(1, f'{count}\n'.encode())
                    if line.startswith('\\echo '):
                        marker = line.split()[1].encode() + b'\n'
                        os.write(1, marker[:9])
                        os.write(1, marker[9:])
            '''))
            fake.chmod(0o700)
            # allow interpreter startup without relaxing the explicit timeout case.
            with profile.Session(fake, root / 'ok.log', 'gin', timeout=5) as session:
                self.assertEqual(session.execute('SELECT 1;'), '1')
                self.assertEqual(session.execute('SELECT 2;'), '2')
                with self.assertRaises(RuntimeError):
                    session.execute('FAIL;')
            with profile.Session(fake, root / 'timeout.log', 'gin', timeout=0.05) as session:
                with self.assertRaises(TimeoutError):
                    session.execute('SLEEP;')

    def test_snapshot_and_ci_contracts(self):
        source = Path(profile.__file__).read_text()
        self.assertIn('BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY', source)
        self.assertIn('SET TRANSACTION SNAPSHOT', source)
        self.assertIn('TIMING OFF', source)
        self.assertIn('TIMING ON', source)
        self.assertIn('cpu_unavailable', source)
        self.assertNotIn('pg_stat_reset', source)
        self.assertEqual(profile.SETTINGS['max_parallel_workers_per_gather'], '0')
        self.assertEqual(profile.SETTINGS['pin.enable_grouped_storage'], 'off')
        driver = Path('tools/g9_qualification.py').read_text()
        self.assertIn("select_rows('g9_small', 'beta AND NOT missingplanet')", driver)
        self.assertIn("query = 'alpha AND NOT missingplanet'", driver)


if __name__ == '__main__':
    unittest.main()
