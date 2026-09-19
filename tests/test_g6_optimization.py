"""G6 source tripwires and a scalar model, not Rust/SQL execution evidence."""
from pathlib import Path
import random
import unittest

ROOT = Path(__file__).resolve().parents[1]
MASK = (1 << 64) - 1


def run_count_model(words: list[int]) -> int:
    """Model fixed-width Rust shifts including truncation of the high bit."""
    carry = 0
    result = 0
    for word in words:
        predecessors = ((word << 1) & MASK) | carry
        result += (word & (~predecessors & MASK)).bit_count()
        carry = word >> 63
    return result


def coordinate_reference(words: list[int]) -> int:
    """Count false-to-true transitions one coordinate at a time."""
    previous = False
    runs = 0
    for word in words:
        for bit in range(64):
            present = bool(word & (1 << bit))
            if present and not previous:
                runs += 1
            previous = present
    return runs


class RunCountModelTests(unittest.TestCase):
    def test_exhaustive_sixteen_bit_patterns(self):
        for pattern in range(1 << 16):
            expected = sum(
                bool(pattern & (1 << bit))
                and (bit == 0 or not pattern & (1 << (bit - 1)))
                for bit in range(16)
            )
            self.assertEqual(run_count_model([pattern, 0]), expected)

    def test_every_domain_and_word_carry(self):
        randomizer = random.Random(0x50494E06)
        for domain in range(1, 513):
            for bits in [(1 << domain) - 1, 0, randomizer.getrandbits(domain)]:
                words = [(bits >> shift) & MASK for shift in range(0, 512, 64)]
                self.assertEqual(run_count_model(words), coordinate_reference(words))
        self.assertEqual(run_count_model([1 << 63, 1]), 1)
        self.assertEqual(run_count_model([MASK] * 8), 1)
        self.assertEqual(run_count_model([0x5555555555555555] * 8), 256)


class RecheckSourceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.rust = (ROOT / 'crates/pin-pg/src/count.rs').read_text()
        cls.core = (ROOT / 'crates/pin-core/src/recheck.rs').read_text()

    def test_experiment_defaults_off_and_is_privileged(self):
        self.assertIn(
            'static ENABLE_COUNT_RECHECK: GucSetting<bool> = GucSetting::new(false);',
            self.rust,
        )
        self.assertIn('GucRegistry::define_bool_guc(', self.rust)
        self.assertIn('c"pin.enable_count_recheck"', self.rust)
        self.assertIn('GucContext::Suset', self.rust)
        self.assertIn('let streaming = ENABLE_COUNT_RECHECK.get();', self.rust)

    def test_sequential_sql_keeps_the_independent_document_oracle(self):
        matching = (ROOT / 'crates/pin-pg/src/matching.rs').read_text()
        self.assertIn('oracle::matches(', matching)
        self.assertNotIn('SingleTermMatcher', matching)
        self.assertIn('oracle::matches(', self.rust)
        self.assertIn('if query.node_count() != 1', self.core)
        self.assertIn('Kind::Term(term)', self.core)

    def test_borrowed_match_still_follows_visibility_and_precedes_clear(self):
        body = self.rust.split('for root in &fallback[..fetches]', 1)[1]
        self.assertLess(body.index('pin_count_fetch(context'), body.index('self.matches(text)'))
        self.assertLess(body.index('self.matches(text)'), body.index('pin_count_clear(context)'))
        self.assertLess(body.index('pin_count_clear(context)'), body.index('if matched?'))
        self.assertIn('#![forbid(unsafe_code)]', (ROOT / 'crates/pin-core/src/lib.rs').read_text())
        self.assertNotIn('unsafe', self.core)

    def test_ascii_does_not_skip_the_profile_or_full_tail_validation(self):
        self.assertLess(self.core.index('check_input(text, limits)?'), self.core.index('text.is_ascii()'))
        self.assertLess(self.core.index('text.is_ascii()'), self.core.index('eq_ignore_ascii_case'))
        self.assertIn('profile_text(text, limits.normalized_bytes, &mut budget)?', self.core)
        body = self.core.split('fn match_words(', 1)[1]
        self.assertIn('for word in text.unicode_words()', body)
        self.assertIn('tokens == limits.tokens', body)
        self.assertNotIn('break;', body)
        self.assertLess(body.index('word.len() > limits.term_bytes'), body.index('work.charge(1)?'))
        self.assertNotIn('split_whitespace', body)
        self.assertIn('max_steps.saturating_sub(1)', body)
        self.assertIn('comparisons_left != 0', body)

    def test_new_sql_is_in_the_existing_disposable_cluster_runner(self):
        runner = (ROOT / 'tools/g2_qualification.sh').read_text()
        self.assertIn('"${psql[@]}" -f "$root/tests/sql/g6_recheck.sql"', runner)
        sql = (ROOT / 'tests/sql/g6_recheck.sql').read_text()
        self.assertIn("set_config('pin.enable_count_vm', 'off', true)", sql)
        self.assertIn("ARRAY['off', 'on', 'off']", sql)
        self.assertIn("current_setting('pin.enable_count_recheck')", sql)
        self.assertIn('EXPLAIN (ANALYZE, FORMAT JSON)', sql)


if __name__ == '__main__':
    unittest.main()
