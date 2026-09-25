"""Independent retirement model and source tripwires, not PostgreSQL MVCC simulation."""
from itertools import combinations
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


def schedules():
    # each thread's order is fixed; exhaust all cross-thread interleavings.
    for slots in combinations(range(6), 3):
        r = iter(('capture', 'decide', 'release'))
        v = iter(('retire', 'reuse', 'publish'))
        yield tuple(next(r) if i in slots else next(v) for i in range(6))


def protected_membership(order, guard):
    held = False
    live = True
    incarnation = 1
    captured = None
    for step in order:
        if step == 'capture':
            held = guard
            captured = incarnation if live else None
        elif step == 'decide':
            if captured is not None and captured != incarnation:
                return False
        elif step == 'release':
            held = False
        elif step == 'retire':
            if held:
                return None  # the lock blocks this schedule before retirement.
            live = False
        elif step == 'reuse':
            assert not live
            incarnation = 2
        elif step == 'publish':
            pass  # publication of the new owner never relives the old snapshot bit.
    return True


class CountModel(unittest.TestCase):
    def test_retirement_interlock_preserves_the_chosen_membership_proof(self):
        self.assertTrue(all(protected_membership(s, True) is not False for s in schedules()))
        # this is a membership counterexample, not a claim about PostgreSQL VM ordering.
        self.assertTrue(any(protected_membership(s, False) is False for s in schedules()))

    def test_retirement_is_required_even_when_the_reused_page_is_all_visible(self):
        old_terms = {'alpha', 'beta'}
        new_terms = {'beta'}
        wanted = {'alpha', 'beta'}
        expected = wanted <= new_terms
        for retired in (True, False):
            old_live = not retired
            grouped = old_live and wanted <= old_terms
            frontier = wanted <= new_terms
            actual = grouped or frontier
            self.assertEqual(actual == expected, retired)

    def test_negative_queries_use_generation_liveness_not_term_union(self):
        universe = {1, 2, 3}
        alpha = {1, 2}
        live = {2, 3}
        self.assertEqual(live - alpha, {3})
        self.assertNotEqual(universe - (alpha & live), live - alpha)


class CountBoundary(unittest.TestCase):
    def test_default_off_and_cached_fallback(self):
        c = (ROOT / 'crates/pin-pg/cshim/pin_count.c').read_text()
        self.assertIn('static bool pin_enable_grouped_count = false;', c)
        self.assertIn('&pin_enable_grouped_count, false, PGC_SUSET', c)
        self.assertIn('grouped count disabled at execution', c)
        self.assertIn('ExecProcNode(state->fallback)', c)
        self.assertLess(c.index('pin_liveness_lock(state->index, false)'),
                        c.index('count = pin_count_group_execute'))

    def test_vacuum_takes_retirement_before_writer_and_releases_after(self):
        rust = (ROOT / 'crates/pin-pg/src/parallel.rs').read_text().split('fn with_vacuum', 1)[1]
        self.assertLess(rust.index('pin_liveness_lock(index, true)'), rust.index('storage::with_writer'))
        self.assertLess(rust.index('storage::with_writer'), rust.index('pin_liveness_unlock(index, true)'))

    def test_root_fallbacks_never_inherit_vm_certification(self):
        rust = (ROOT / 'crates/pin-pg/src/count_grouped.rs').read_text()
        root = rust.split('fn root(', 1)[1].split('fn page(', 1)[0]
        self.assertIn('self.heap(root, recheck)', root)
        self.assertNotIn('all_visible', root)
        page = rust.split('fn page(', 1)[1].split('fn work(', 1)[0]
        self.assertIn('pin_count_group_all_visible', page)
        self.assertIn('self.heap(root, false)', page)
        self.assertIn('word.count_ones()', page)

    def test_bridge_counter_extents_match(self):
        c = (ROOT / 'crates/pin-pg/cshim/pin_count.h').read_text()
        rust = (ROOT / 'crates/pin-pg/src/count_grouped.rs').read_text()
        self.assertIn('PIN_GROUP_COUNT_STATS 11', c)
        self.assertIn('const COUNTERS: usize = 11;', rust)
        self.assertIn('length > isize::MAX as usize', rust)
        self.assertIn('native::call(|| pin_count_clear(context))', rust)

    def test_native_driver_is_invoked_and_has_real_reuse_and_lock_checks(self):
        runner = (ROOT / 'tools/g9_qualification.sh').read_text()
        sql = (ROOT / 'tests/sql/g9_count.sql').read_text()
        driver = (ROOT / 'tools/g9_count.py').read_text()
        self.assertIn('tools/g9_count.py', runner)
        self.assertIn('r.ctid = o.old_tid', sql)
        self.assertIn('pg_stat_xact_user_tables', sql)
        self.assertIn("l.page = 2", driver)
        self.assertIn('cluster.restart(immediate=True)', driver)
        self.assertIn('REPEATABLE READ', driver)
        self.assertIn('pg_cancel_backend', driver)
        self.assertIn('pg_terminate_backend', driver)


if __name__ == '__main__':
    unittest.main()
