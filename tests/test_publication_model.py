"""Tests for the executable independent model, not Rust execution."""
from pathlib import Path
import unittest
from tools.publication_model import ACTIONS, explore, fixed_point, invariant, report, successor


class PublicationModelTests(unittest.TestCase):
    def test_graph_matches_checked_in_rust_fixture(self):
        root = Path(__file__).resolve().parents[1]
        expected = root / 'crates/pin-core/tests/fixtures/publication-states.txt'
        self.assertEqual(sorted(fixed_point()), list(map(int, expected.read_text().split())))
        result = report()
        self.assertEqual(result['states'], 37)
        self.assertEqual(result['non_stuttering_edges'], 76)

    def test_all_edges_preserve_the_corrected_invariant(self):
        states, edges, failure = explore()
        self.assertIsNone(failure)
        self.assertTrue(all(edges))
        for state in states:
            for action in range(len(ACTIONS)):
                nxt = successor(state, action)
                if nxt is not None:
                    self.assertIn(nxt, states)
                    self.assertTrue(invariant(nxt))
                    self.assertNotEqual(state, nxt)

    def test_negative_controls_replay_to_real_violations(self):
        for reconcile, retired in [(False, True), (True, False)]:
            trace = explore(reconcile, retired)[2]
            self.assertIsNotNone(trace)
            broken = corrected = 0
            for name in trace:
                action = ACTIONS.index(name)
                broken = successor(broken, action, reconcile, retired)
                self.assertIsNotNone(broken)
                if corrected is not None:
                    corrected = successor(corrected, action)
            self.assertFalse(invariant(broken))
            self.assertTrue(corrected is None or invariant(corrected))

    def test_partial_documents_never_publish(self):
        for fragments in range(3):
            self.assertIsNone(successor(fragments, ACTIONS.index('publish')))


if __name__ == '__main__':
    unittest.main()
