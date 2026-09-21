"""G7 harness checks and source tripwires, not PostgreSQL execution evidence."""
import copy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tools.g7_qualification import Cluster, nodes, predicate, require_bitmap, settings

ROOT = Path(__file__).resolve().parents[1]


def plan(parallel=True):
    heap = {'Node Type': 'Bitmap Heap Scan', 'Relation Name': 'docs',
            'Parallel Aware': parallel, 'Recheck Cond': 'body @@@ query',
            'Lossy Heap Blocks': 3, 'Workers': [{'Worker Number': 0, 'Lossy Heap Blocks': 5}],
            'Plans': [{'Node Type': 'Bitmap Index Scan', 'Index Name': 'docs_pin'}]}
    return {'Node Type': 'Gather', 'Workers Planned': 2, 'Workers Launched': 1,
            'Plans': [heap]} if parallel else heap


class PlanTests(unittest.TestCase):
    def test_worker_local_block_counters_are_counted_once(self):
        result = require_bitmap(plan(), parallel=True, lossy=True)
        self.assertEqual(result, {'workers_launched': 1, 'lossy_heap_blocks': 8})
        self.assertEqual(len(list(nodes(plan()))), 3)

    def test_planned_workers_are_not_execution_evidence(self):
        value = plan()
        value['Workers Launched'] = 0
        with self.assertRaisesRegex(RuntimeError, 'actually launch'):
            require_bitmap(value, parallel=True)
        with self.assertRaisesRegex(RuntimeError, 'actually launch'):
            require_bitmap(plan(False), parallel=True)

    def test_parallel_heap_and_serial_reference_are_distinct(self):
        with self.assertRaisesRegex(RuntimeError, 'serial reference'):
            require_bitmap(plan(), parallel=False)
        value = plan(False)
        self.assertEqual(require_bitmap(value, parallel=False)['workers_launched'], 0)
        value = plan()
        value['Plans'][0]['Parallel Aware'] = False
        with self.assertRaisesRegex(RuntimeError, 'actually launch'):
            require_bitmap(value, parallel=True)

    def test_wrong_missing_and_duplicate_index_nodes_fail(self):
        for name in ['another_index', None]:
            value = plan()
            value['Plans'][0]['Plans'][0]['Index Name'] = name
            with self.assertRaisesRegex(RuntimeError, 'one Pin bitmap'):
                require_bitmap(value, parallel=True)
        value = plan()
        value['Plans'][0]['Plans'].append(copy.deepcopy(value['Plans'][0]['Plans'][0]))
        with self.assertRaisesRegex(RuntimeError, 'one Pin bitmap'):
            require_bitmap(value, parallel=True)

    def test_recheck_is_not_optional(self):
        value = plan()
        del value['Plans'][0]['Recheck Cond']
        with self.assertRaisesRegex(RuntimeError, 'recheck condition'):
            require_bitmap(value, parallel=True)

    def test_custom_count_cannot_replace_native_parallel(self):
        value = plan()
        value['Custom Plan Provider'] = 'PinCount'
        with self.assertRaisesRegex(RuntimeError, 'custom count'):
            require_bitmap(value, parallel=True)

    def test_low_and_high_memory_paths_must_be_observed(self):
        with self.assertRaisesRegex(RuntimeError, 'unexpectedly became lossy'):
            require_bitmap(plan(), parallel=True, lossy=False)
        value = plan()
        heap = value['Plans'][0]
        heap['Lossy Heap Blocks'] = 0
        heap['Workers'][0]['Lossy Heap Blocks'] = 0
        require_bitmap(value, parallel=True, lossy=False)
        with self.assertRaisesRegex(RuntimeError, 'did not exercise lossy'):
            require_bitmap(value, parallel=True, lossy=True)

    def test_reference_settings_do_not_enable_custom_shortcuts(self):
        value = settings(False, sequential=True)
        self.assertIn('max_parallel_workers_per_gather = 0', value)
        self.assertIn('enable_bitmapscan = off', value)
        self.assertIn('pin.enable_count_fastpath = off', value)
        self.assertIn('pin.enable_count_vm = off', value)
        self.assertIn('parallel_leader_participation = off', settings(True, leader=False))
        self.assertIn("'a''b'", predicate("a'b"))
        self.assertTrue(predicate('common', True).endswith('AND keep AND id % 7 <> 0'))

    def test_sql_is_sent_as_top_level_commands_not_one_implicit_transaction(self):
        with tempfile.TemporaryDirectory() as path:
            cluster = Cluster('/fixture/psql', Path(path))
            with patch('tools.g7_qualification.subprocess.run') as run:
                run.return_value.returncode = 0
                run.return_value.stdout = ''
                run.return_value.stderr = ''
                cluster.run('VACUUM fixture;')
                self.assertNotIn('-c', run.call_args.args[0])
                self.assertIn('VACUUM fixture;', run.call_args.kwargs['input'])
                self.assertTrue((Path(path) / '001.sql').exists())


class SourceTests(unittest.TestCase):
    def test_storage_experiment_defaults_off_and_is_privileged(self):
        source = (ROOT / 'crates/pin-pg/src/maintenance.rs').read_text()
        self.assertIn('GucSetting::<bool>::new(false)', source)
        self.assertIn('GucContext::Suset', source)
        self.assertIn('CompactMode::Copy', source)
        self.assertIn('stats.retained_pages', source)

    def test_retained_boundary_and_retirement_are_published_together(self):
        source = (ROOT / 'crates/pin-core/src/mutable/compact.rs').read_text()
        self.assertIn('head: rewrite_head,', source)
        self.assertIn('store.commit(&[meta, &dictionary, &boundary])?', source)
        self.assertIn('boundary.next()? != rewrite_head', source)
        self.assertIn('compact_with_mode(store, CompactMode::Copy)', source)
        self.assertNotIn('unsafe ', source)
        self.assertNotIn('Vec<', source)

    def test_parallel_build_does_not_overadvertise_index_scan_callbacks(self):
        source = (ROOT / 'crates/pin-pg/src/am.rs').read_text()
        self.assertIn('amcanparallel: false', source)
        self.assertIn('amcanbuildparallel: true', source)
        self.assertIn('amusemaintenanceworkmem: false', source)
        for callback in ['amestimateparallelscan', 'aminitparallelscan', 'amparallelrescan']:
            self.assertIn(f'{callback}: None', source)

    def test_parallel_count_uses_fixed_pointer_free_work(self):
        rust = (ROOT / 'crates/pin-pg/src/count.rs').read_text()
        core = (ROOT / 'crates/pin-core/src/mutable/work.rs').read_text()
        host = (ROOT / 'crates/pin-pg/cshim/pin_count.c').read_text()
        self.assertIn('WorkState::capture', rust)
        self.assertIn('pin_count_work_claim', rust)
        self.assertIn('pub const WORK_WORDS: usize = 11;', core)
        self.assertNotIn('*mut', core)
        self.assertIn('slock_t mutex;', host)
        self.assertIn('CreateParallelContext', host)
        self.assertIn('work_mem', host)
        self.assertIn('pin_count_participant_memory', host)

    def test_real_host_suites_use_existing_driver(self):
        source = (ROOT / 'tools/g2_qualification.sh').read_text()
        self.assertIn('tools/g7_qualification.py', source)
        self.assertIn('tools/g7_recovery.sh', source)
        self.assertIn('--disposable', source)
        self.assertIn('fsync = on', source)
        self.assertIn('full_page_writes = on', source)


if __name__ == '__main__':
    unittest.main()
