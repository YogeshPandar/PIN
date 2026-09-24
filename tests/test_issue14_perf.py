"""Offline tests of evidence accounting; no PostgreSQL measurements are implied."""
import importlib.util
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
SPEC = importlib.util.spec_from_file_location('issue14_perf', ROOT / 'tools/issue14_perf.py')
assert SPEC is not None and SPEC.loader is not None
PERF = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PERF)


class PerformanceEvidenceTests(unittest.TestCase):
    def test_backend_stat_fields_ignore_spaces_and_parentheses_in_comm(self):
        fields = ['S'] + ['0'] * 30
        for index, value in {7: 11, 9: 2, 11: 120, 12: 30, 19: 5000, 21: 8192}.items():
            fields[index] = str(value)
        result = PERF.parse_proc_stat('42 (postgres (worker)) ' + ' '.join(fields))
        self.assertEqual(result, {'pid': 42, 'minor_faults': 11, 'major_faults': 2,
                                  'user_ticks': 120, 'system_ticks': 30,
                                  'start_ticks': 5000, 'rss_pages': 8192})
        with self.assertRaises(ValueError):
            PERF.parse_proc_stat('42 (python) ' + ' '.join(fields))
        with self.assertRaises(ValueError):
            PERF.parse_proc_stat('42 (postgres) S 0')

    def test_cpu_uses_backend_ticks_not_wall_time(self):
        before = {'pid': 1, 'start_ticks': 5, 'user_ticks': 10, 'system_ticks': 20,
                  'minor_faults': 3, 'major_faults': 0, 'rss_pages': 100}
        after = {**before, 'user_ticks': 130, 'system_ticks': 50, 'rss_pages': 101}
        result = PERF.cpu_delta(before, after, 100, 1000)
        self.assertEqual(result['backend_cpu_ms_per_query'], 1.5)
        self.assertEqual(result['one_tick_ms_per_query'], 0.01)
        self.assertTrue(result['cpu_resolution_qualified'])
        self.assertFalse(PERF.cpu_delta(before, before, 100, 1000)['cpu_resolution_qualified'])
        for changes in ({'pid': 2}, {'start_ticks': 6}, {'user_ticks': 0}):
            with self.assertRaises(ValueError):
                PERF.cpu_delta(before, {**after, **changes}, 100, 1000)

    def test_unstable_controls_are_never_qualified(self):
        self.assertFalse(PERF.control_pair(100, 130, 0.1)['stable'])
        self.assertFalse(PERF.control_pair(130, 100, 0.1)['stable'])
        self.assertTrue(PERF.control_pair(100, 105, 0.1)['stable'])
        for a, b, tolerance in [(0, 10, .1), (10, float('nan'), .1), (10, 10, -1)]:
            with self.assertRaises(ValueError):
                PERF.control_pair(a, b, tolerance)

    def test_explain_does_not_double_count_inclusive_buffers(self):
        index = {'Node Type': 'Bitmap Index Scan', 'Index Name': 'fixture_pin',
                 'Actual Loops': 1, 'Actual Total Time': 2, 'Shared Hit Blocks': 20}
        heap = {'Node Type': 'Bitmap Heap Scan', 'Actual Loops': 1, 'Actual Total Time': 5,
                'Shared Hit Blocks': 100, 'Rows Removed by Index Recheck': 3, 'Plans': [index]}
        root = {'Node Type': 'Aggregate', 'Actual Total Time': 6, 'Actual Loops': 1,
                'Shared Hit Blocks': 100, 'Plans': [heap]}
        plan = [{'Plan': root, 'Planning Time': 0.2, 'Execution Time': 6.1}]
        result = PERF.plan_summary(plan, 'public.fixture_pin')
        self.assertEqual(result['root_buffers']['Shared Hit Blocks'], 100)
        self.assertEqual(result['index_elapsed_ms'], 2)
        self.assertEqual(result['nodes_inclusive'][1]['Rows Removed by Index Recheck'], 3)
        self.assertTrue(result['elapsed_is_not_cpu'])
        with self.assertRaises(ValueError):
            PERF.plan_summary(plan, 'other_index')
        root['Node Type'] = 'Gather'
        with self.assertRaises(ValueError):
            PERF.plan_summary(plan, 'fixture_pin')

    def test_table_identifiers_and_literals_are_explicit(self):
        self.assertEqual(PERF.identifier('public.pin_g6_bench'), '"public"."pin_g6_bench"')
        self.assertEqual(PERF.literal("can't"), "'can''t'")
        for invalid in ['a; DROP TABLE b', 'a.b.c', '', 'a--', 'a"b', 'MixedCase']:
            with self.assertRaises(ValueError):
                PERF.identifier(invalid)

    def test_three_engines_only_toggle_the_group_scan_gate(self):
        for engine, value in [('legacy', 'off'), ('grouped', 'on'), ('gin', 'off')]:
            options = PERF.environment(engine)['PGOPTIONS']
            self.assertIn(f'pin.enable_grouped_scan={value}', options)
            self.assertIn('default_transaction_read_only=on', options)
            self.assertIn('max_parallel_workers_per_gather=0', options)
            self.assertIn('pin.enable_grouped_storage=off', options)


if __name__ == '__main__':
    unittest.main()
