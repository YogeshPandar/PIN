"""Executable finite models; Rust tests remain separate native gates."""
from itertools import product
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
from issue14_model import candidates, cover, matches, phrase_oracle, phrase_ring

A, B, C = [('term', name) for name in 'abc']
EXPRESSIONS = [
    ('and', A, B), ('and', B, A), ('or', A, B),
    ('and', A, ('or', B, C)), ('or', ('and', A, B), C),
    ('and', ('or', A, B), ('or', B, C)), ('and', A, ('not', B)),
    ('and', ('or', A, ('not', B)), C), ('and', A, ('not', ('and', B, C))),
    ('or', ('and', A, ('not', B)), ('and', C, ('not', A))),
    ('and', A, ('or', ('term', 'missing'), B)),
    ('not', A), ('or', A, ('not', B)), ('not', ('not', A)),
]


class FiniteModelTests(unittest.TestCase):
    def test_all_four_group_three_term_assignments_have_no_missed_matches(self):
        universe = set(range(4))
        subsets = [{i for i in universe if mask & (1 << i)} for mask in range(16)]
        for values in product(subsets, repeat=3):
            terms = dict(zip('abc', values))
            for expr in EXPRESSIONS:
                expected = matches(expr, terms, universe)
                actual, _ = candidates(expr, terms, universe)
                self.assertEqual(actual, sorted(set(actual)))
                self.assertLessEqual(expected, set(actual), (expr, terms, expected, actual))
                if cover(expr) is not None and 'not' not in repr(expr):
                    self.assertEqual(set(actual), expected)

    def test_selective_conjunction_is_operand_order_independent(self):
        universe = set(range(130))
        terms = {'a': universe, 'b': {129}}
        for expr in [('and', A, B), ('and', B, A)]:
            groups, probes = candidates(expr, terms, universe)
            self.assertEqual(groups, [129])
            self.assertEqual(probes, 3)
        self.assertEqual(candidates(('or', A, B), terms, universe)[0], sorted(universe))

    def test_disjoint_streams_terminate_without_payload_candidates(self):
        universe = set(range(130))
        groups, _ = candidates(('and', A, B), {'a': set(range(0, 130, 2)),
                                             'b': set(range(1, 130, 2))}, universe)
        self.assertEqual(groups, [])

    def test_phrase_ring_preserves_windows_comparison_counts_and_budget(self):
        for width in range(1, 5):
            for phrase in product('ab', repeat=width):
                for length in range(7):
                    for words in product('ab', repeat=length):
                        for budget in range(1 + length * width + 2):
                            self.assertEqual(phrase_ring(list(words), list(phrase), budget),
                                             phrase_oracle(list(words), list(phrase), budget))


if __name__ == '__main__':
    unittest.main()
