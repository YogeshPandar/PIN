"""Historical work attribution must not masquerade as a new CPU measurement."""
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from tools.g10_plan_costs import plan_work, ratio, reanalyze


class PlanCosts(unittest.TestCase):
    def test_excludes_unexecuted_fallback(self):
        fallback = {'Custom Plan Provider': 'PinCount', 'Actual Loops': 0, 'Heap Fetches': 999}
        current = {'Custom Plan Provider': 'PinCount', 'Actual Loops': 1, 'Heap Fetches': 3,
                   'Candidate Owners': 10, 'VM Probes': 4, 'Plans': [fallback]}
        result = plan_work([{'Plan': current}])
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]['heap_fetch_fraction'], 0.3)
        self.assertEqual(result[0]['vm_probes_per_candidate'], 0.4)
        self.assertIsNone(result[0]['payload_to_decoded_offset_bytes'])

    def test_rejects_invalid_counters_and_ratios(self):
        for value in (-1, float('nan'), float('inf'), '12', True):
            with self.assertRaises(ValueError):
                plan_work([{'Plan': {'Custom Plan Provider': 'PinCount', 'Actual Loops': 1,
                                     'Heap Fetches': value}}])
        self.assertIsNone(ratio(0, 0))
        with self.assertRaises(ValueError):
            ratio(1, -1)

    def test_preserves_failing_stages_and_control_provenance(self):
        rows = [dict(rows=20, stage=stage, case='and', mode=mode, median_batch_cpu_us=cpu)
                for stage, mode, cpu in [('fresh', 'gin', 100), ('fresh', 'pin_grouped', 9),
                                        ('long_delta', 'gin', 100), ('long_delta', 'pin_grouped', 200)]]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'run.tar.gz'
            with tarfile.open(path, 'w:gz') as archive:
                data = json.dumps(rows).encode()
                member = tarfile.TarInfo('run/summary.json')
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
            result = reanalyze(path)
        self.assertEqual([r['at_most_ten_percent'] for r in result['comparisons']], [True, False])
        self.assertIn('not-new-performance', result['kind'])
        self.assertEqual(len(result['archive_sha256']), 64)


if __name__ == '__main__':
    unittest.main()
