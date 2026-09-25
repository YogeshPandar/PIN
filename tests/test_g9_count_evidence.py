"""Validate historical extraction against committed controls, not new results."""
from pathlib import Path
import unittest
from tools.g9_count_evidence import reanalyze


class HistoricalEvidenceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.result = reanalyze(Path(__file__).resolve().parents[1])

    def test_historical_rare_label_keeps_actual_selectivity(self):
        rare = next(x for x in self.result['controls'] if x['historical_label'] == 'rare')
        self.assertEqual(rare['modes']['on']['matched_rows'], [8212])
        self.assertAlmostEqual(rare['selectivity'][0], 8212/25576)
        self.assertLess(rare['owner_on_speedup_vs_gin'], 1)

    def test_every_cpu_class_keeps_all_controls_and_sample_denominators(self):
        self.assertEqual(len(self.result['controls']), 4)
        for row in self.result['controls']:
            self.assertEqual(set(row['modes']), {'off', 'on', 'gin'})
            for mode in row['modes'].values():
                self.assertEqual(mode['samples'], 4)
                self.assertEqual(mode['queries'], [100]*4)

    def test_profiles_and_maintenance_have_raw_hashes_and_are_not_new_measurements(self):
        self.assertEqual(self.result['kind'], 'historical-reanalysis-not-new-performance')
        self.assertEqual(len(self.result['source']['member_sha256']), 64)
        for profile in self.result['sampled_profiles']:
            self.assertEqual(len(profile['flat_sha256']), 64)
            self.assertTrue(profile['top_15_self_symbols'])
        for mode in self.result['maintenance']:
            self.assertGreater(mode['pin_over_gin']['backend_cpu_ns'], 2)
            self.assertGreater(mode['pin_over_gin']['wal_bytes'], 4)
            self.assertEqual(mode['pin_over_gin']['index_bytes'], 2.5)


if __name__ == '__main__':
    unittest.main()
