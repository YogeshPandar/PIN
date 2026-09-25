"""Independent protocol/visibility oracle, including deliberately broken schedules."""
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
import g9_count_model as model


class CountModelTests(unittest.TestCase):
    def test_every_correct_schedule_agrees_with_snapshot_oracle(self):
        results = model.run()
        self.assertTrue(all(item['failure'] is None for item in results['correct']))
        self.assertGreater(sum(item['fallback_states'] for item in results['correct']), 0)

    def test_each_broken_protocol_has_a_replayable_witness(self):
        for item in model.run()['negative_controls']:
            self.assertIsNotNone(item['failure'], item['mode'])
            self.assertTrue(item['failure']['trace'])
            case = next(case for case in model.CASES if case.name == item['case'])
            state = model.State(live=case.live, heap_present=case.live, vm=case.vm)
            for action in item['failure']['trace']:
                state = dict(model.transitions(case, state, item['mode']))[action]
            self.assertEqual(model.violation(case, state), item['failure']['reason'])

    def test_tid_only_union_is_not_boolean_identity(self):
        result = model.generation_oracle()
        self.assertFalse(result['exact_and'])
        self.assertTrue(result['broken_tid_union_and'])


if __name__ == '__main__':
    unittest.main()
