"""Measurement-contract checks; these do not run PostgreSQL benchmarks."""
from collections import Counter
from copy import deepcopy
import unittest
from tools import g9_count_bench as bench


class PairedCountBench(unittest.TestCase):
    def test_every_position_and_order_retains_gin(self):
        order = bench.orders(12)
        self.assertEqual(len(set(order)), 6)
        for row in order:
            self.assertEqual(set(row), set(bench.MODES))
        for position in range(3):
            self.assertEqual(Counter(row[position] for row in order),
                             Counter({mode: 4 for mode in bench.MODES}))
        for invalid in (0, 4, 7, 61):
            with self.assertRaises(ValueError):
                bench.orders(invalid)

    def test_ranking_uses_identical_postgres_scoring_and_ties(self):
        left = bench.sql_for('ranked_topk', 'pin_grouped')
        right = bench.sql_for('ranked_topk', 'gin')
        self.assertEqual(left.split(' WHERE ')[0], right.split(' WHERE ')[0])
        self.assertEqual(left.split(' ORDER BY ')[1], right.split(' ORDER BY ')[1])
        self.assertIn('ts_rank_cd', left)
        self.assertIn('score DESC,id LIMIT 20', left)
        self.assertNotIn('count(*)', bench.sql_for('broad_rows', 'pin_grouped'))

    def test_new_deltas_do_not_make_rare_term_common(self):
        self.assertIn('rareplanet', bench.body_sql())
        self.assertNotIn('rareplanet', bench.body_sql(rare=False))
        self.assertIn('65537', bench.body_sql())
        with self.assertRaises(ValueError):
            bench.body_sql('untrusted;')

    def test_ns_backend_cpu_and_identity_checks(self):
        before = dict(pid=100, start=7, cpu_ns=10, runqueue_ns=4)
        after = dict(pid=100, start=7, cpu_ns=1_000_010, runqueue_ns=14)
        self.assertEqual(bench.cpu_delta(before, after, 2)['cpu_us_per_query'], 500)
        for bad in ({**after, 'start': 8}, {**after, 'cpu_ns': 5}):
            with self.assertRaises(ValueError):
                bench.cpu_delta(before, bad, 2)

    def test_plan_must_prove_grouped_execution_not_just_enablement(self):
        plan = [{'Plan': {'Node Type': 'Custom Scan', 'Custom Plan Provider': 'PinCount',
                         'Grouped Count Runs': 1, 'Plans': [{'Node Type': 'Bitmap Heap Scan',
                         'Plans': [{'Node Type': 'Bitmap Index Scan',
                                    'Index Name': 'documents_pin'}]}]}}]
        self.assertEqual(bench.check_plan(plan, 'pin_grouped', 'broad_and_count', executed=True)
                         ['grouped_runs'], 1)
        bad = deepcopy(plan)
        bad[0]['Plan']['Grouped Count Runs'] = 0
        with self.assertRaisesRegex(ValueError, 'was not executed'):
            bench.check_plan(bad, 'pin_grouped', 'broad_and_count', executed=True, require_grouped=True)
        for mode, case in (('gin', 'broad_and_count'), ('pin_grouped', 'phrase_count')):
            with self.assertRaises(ValueError):
                bench.check_plan(plan, mode, case, executed=True)

    def test_large_row_output_is_received_not_replaced_by_aggregate(self):
        class Fake:
            sql = ''
            def execute(self, sql):
                self.sql = sql
        session = Fake()
        bench.run_result(session, 'broad_rows', 'pin_grouped')
        self.assertIn('EXECUTE measured_pin_grouped;', session.sql)
        self.assertIn('\\o /dev/null', session.sql)
        self.assertNotIn('count', session.sql)


if __name__ == '__main__':
    unittest.main()
