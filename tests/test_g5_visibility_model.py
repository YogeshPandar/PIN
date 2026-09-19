"""Frozen graph sizes and replayable negative controls for the G5 proof model."""
import unittest

from tools.g5_visibility_model import CASES, State, explore, run, transitions, violation


class VisibilityModelTests(unittest.TestCase):
    def test_correct_graphs_cover_all_bounded_schedules(self):
        expected = [
            (65, 103, 9),
            (30, 30, 5),
            (49, 86, 6),
            (9, 10, 4),
            (9, 10, 4),
            (9, 10, 4),
        ]
        for case, counts in zip(CASES, expected, strict=True):
            with self.subTest(case=case.name):
                result = explore(case)
                self.assertEqual(
                    tuple(result[key] for key in ("states", "transitions", "terminal")),
                    counts,
                )
                self.assertIsNone(result["failure"])

    def test_each_broken_protocol_has_a_replayable_counterexample(self):
        results = run()
        by_name = {case.name: case for case in CASES}
        for result in results["negative_controls"]:
            case = by_name[result["case"]]
            state = State(live=case.live, heap_present=case.live, vm=case.vm)
            for action in result["failure"]["trace"]:
                choices = [
                    (candidate, following)
                    for candidate, following in transitions(case, state, result["mode"])
                    if candidate == action
                ]
                self.assertEqual(len(choices), 1)
                state = choices[0][1]
            self.assertEqual(violation(case, state), result["failure"]["reason"])

    def test_empty_all_visible_page_does_not_certify_removed_owner(self):
        result = explore(CASES[0], "ignore_owner_pin")
        self.assertIn("vacuum:set_vm_empty", result["failure"]["trace"])
        self.assertFalse(result["failure"]["state"]["heap_present"])
        self.assertEqual(result["failure"]["state"]["result"], 1)

    def test_unknown_protocol_mode_is_an_error(self):
        with self.assertRaises(ValueError):
            explore(CASES[0], "unknown")


if __name__ == "__main__":
    unittest.main()
