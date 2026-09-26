"""Check the G9 driver and boundary contracts without claiming native execution."""

import argparse
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from tools.g9_qualification import Cluster, STAGES, identifier, literal, require_bitmap, select_rows

ROOT = Path(__file__).resolve().parents[1]


class DriverTests(unittest.TestCase):
    def test_fixture_identifiers_are_bounded(self):
        self.assertEqual(identifier('g9_crash'), 'g9_crash')
        for value in ('g9_crash;DROP TABLE x', 'public.g9_crash', 'g9_1', 'docs'):
            with self.assertRaises(ValueError):
                identifier(value)

    def test_literals_and_full_row_results(self):
        self.assertEqual(literal("a'b"), "'a''b'")
        statement = select_rows('g9_crash', "a'b")
        self.assertIn('json_agg(id ORDER BY id)', statement)
        self.assertIn("parse_query('a''b')", statement)
        self.assertNotIn('count(*)', statement)

    def test_nested_bitmap_plan_is_required(self):
        require_bitmap([{'Plan': {'Node Type': 'Aggregate', 'Plans': [
            {'Node Type': 'Bitmap Heap Scan', 'Plans': [{'Node Type': 'Bitmap Index Scan'}]},
        ]}}])
        with self.assertRaises(AssertionError):
            require_bitmap([{'Plan': {'Node Type': 'Seq Scan'}}])

    def test_psql_uses_stdin_so_vacuum_is_not_in_an_implicit_command_batch(self):
        with tempfile.TemporaryDirectory() as temp:
            args = argparse.Namespace(psql='/fixture/psql', artifacts=Path(temp))
            cluster = Cluster(args)
            with patch('tools.g9_qualification.subprocess.run') as run:
                run.return_value = subprocess.CompletedProcess([], 0, '', '')
                cluster.run('SET work_mem = \'4MB\';\nVACUUM g9_crash;')
                self.assertNotIn('-c', run.call_args.args[0])
                self.assertIn('VACUUM g9_crash;', run.call_args.kwargs['input'])
                self.assertTrue((Path(temp) / '001-query.sql').exists())

    def test_expected_error_rejects_success_and_unrelated_errors(self):
        with tempfile.TemporaryDirectory() as temp:
            cluster = Cluster(argparse.Namespace(psql='/fixture/psql', artifacts=Path(temp)))
            for status, message in ((0, ''), (1, 'server unavailable')):
                with patch('tools.g9_qualification.subprocess.run') as run:
                    run.return_value = subprocess.CompletedProcess([], status, '', message)
                    with self.assertRaises(AssertionError):
                        cluster.run('SELECT 1', error='Pin injected storage error')

    def test_expected_injection_is_not_mistaken_for_success(self):
        with tempfile.TemporaryDirectory() as temp:
            cluster = Cluster(argparse.Namespace(psql='/fixture/psql', artifacts=Path(temp)))
            with patch('tools.g9_qualification.subprocess.run') as run:
                run.return_value = subprocess.CompletedProcess([], 3, '', 'Pin injected storage error')
                cluster.run('SELECT 1', error='Pin injected storage error')


class SourceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.host = (ROOT / 'crates/pin-pg/cshim/pin_grouped.c').read_text()
        cls.adapter = (ROOT / 'crates/pin-pg/src/grouped.rs').read_text()
        cls.driver = (ROOT / 'tools/g9_qualification.py').read_text()

    def test_default_off_privileged_independent_gates(self):
        self.assertEqual(self.adapter.count('GucSetting::<bool>::new(false)'), 5)
        self.assertEqual(self.adapter.count('GucContext::Suset'), 5)
        self.assertIn('pin.enable_frontier_anchors', self.adapter)
        self.assertIn('pin.enable_owner_frontier', self.adapter)
        self.assertIn('pin.enable_grouped_storage', self.adapter)
        self.assertIn('pin.enable_grouped_scan', self.adapter)
        self.assertIn('pin.enable_grouped_delta_seal', self.adapter)
        self.assertIn('mutable::scan_query_with_recheck', self.adapter)
        self.assertNotIn('unsafe impl', self.adapter)
        self.assertNotIn('impl Drop', self.adapter)

    def test_sort_uses_native_bounded_forward_only_storage(self):
        for contract in (
            'maintenance_work_mem', 'autovacuum_work_mem', 'AmAutoVacuumWorkerProcess()',
            'reserved_kb + PIN_GROUP_MIN_SORT_KB', 'tuplesort_begin_datum(BYTEAOID, ByteaLessOperator',
            'NULL, TUPLESORT_NONE', 'tuplesort_getdatum(sort->state, true, false',
            'tuplesort_end(sort->state)', 'VARATT_IS_4B_U(datum)', '#include "varatt.h"',
        ):
            self.assertIn(contract, self.host)
        for forbidden in ('malloc(', 'fopen(', 'mkstemp(', 'pthread_', 'mmap('):
            self.assertNotIn(forbidden, self.host)
        self.assertEqual(self.host.count('CHECK_FOR_INTERRUPTS()'), 2)

    def test_ffi_signatures_and_record_width_agree(self):
        build = (ROOT / 'crates/pin-pg/build.rs').read_text()
        for flag in ('-fno-strict-aliasing', '-fwrapv', '-fexcess-precision=standard'):
            self.assertIn('"' + flag + '"', build)
        header = (ROOT / 'crates/pin-pg/cshim/pin_grouped.h').read_text()
        native = (ROOT / 'crates/pin-pg/src/native.rs').read_text()
        for function in ('begin', 'put', 'finish', 'read', 'end'):
            name = 'pin_group_sort_' + function
            self.assertIn(name + '(', header)
            self.assertIn(name + '(', native)
            self.assertIn('native::call(|| native::' + name, self.adapter)
        self.assertIn('size_of::<SortRecord>() == 32', self.adapter)
        self.assertIn('align_of::<SortRecord>() == 1', self.adapter)
        self.assertIn('#define PIN_GROUP_RECORD_BYTES 32', self.host)
        self.assertIn('#define PIN_GROUP_SORT_BATCH 256', self.host)
        self.assertIn('if records.len() > SORT_BATCH', self.adapter)
        self.assertIn('if output.len() > SORT_BATCH', self.adapter)

    def test_core_events_do_not_collide_with_parallel_worker_events(self):
        source = (ROOT / 'crates/pin-core/src/mutable/mod.rs').read_text()
        body = source.split('pub enum Stage {', 1)[1].split('}', 1)[0]
        values = dict((name, int(value)) for name, value in re.findall(r'(\w+)\s*=\s*(\d+)', body))
        self.assertEqual(len(values), len(set(values.values())))
        self.assertTrue(set(range(1, 17)).issubset(set(values.values())))
        self.assertTrue(set(range(17, 20)).isdisjoint(set(values.values())))
        self.assertEqual(values['GroupScan'], 37)
        self.assertEqual(values['GroupSortReady'], 38)
        self.assertEqual(set(STAGES), {32, 33, 34, 35, 36, 38})

    def test_liveness_retirement_is_unconditional_and_precedes_owner_removal(self):
        source = (ROOT / 'crates/pin-core/src/mutable/vacuum.rs').read_text()
        self.assertLess(source.index('super::grouped::retire('), source.index('store.remove_owners('))
        self.assertNotIn('storage_enabled', source)
        self.assertNotIn('enable_grouped', source)

    def test_rebuild_and_bitmap_use_existing_host_barriers(self):
        am = (ROOT / 'crates/pin-pg/src/am.rs').read_text()
        self.assertIn('amusemaintenanceworkmem: true', am)
        self.assertIn('storage_impl::with_maintenance(index', am)
        self.assertIn('storage::with_maintenance(index, strategy', am)
        self.assertIn('crate::grouped::scan(store', am)
        self.assertIn('sink.push(root, recheck || !single_key)', am)
        self.assertIn('if !storage_enabled() || !grouped::needs_rebuild(store)?', self.adapter)
        self.assertLess(self.adapter.index('let spilled = sort.close();'), self.adapter.index('let stats = result?;'))

    def test_crash_driver_flushes_witness_before_immediate_stop(self):
        crash = self.driver.split('def crash_boundaries(', 1)[1].split('def cancellation(', 1)[0]
        self.assertLess(crash.index('INSERT INTO g9_wal_witness'), crash.index('restart(immediate=True)'))
        self.assertIn('wait_lock(paused_app, False)', crash)
        self.assertIn('disabled-recovery', crash)
        shell = (ROOT / 'tools/g9_qualification.sh').read_text()
        for contract in ('fsync = on', 'full_page_writes = on', 'synchronous_commit = on',
                         'shared_preload_libraries', 'mktemp -d /tmp/pin-g9.', 'trap cleanup EXIT'):
            self.assertIn(contract, shell)
        self.assertIn('inspect {log}', self.driver)

    def test_cancellation_observes_live_spill_and_cleanup(self):
        body = self.driver.split('def cancellation(', 1)[1].split(
            'def concurrent_reader_writer_maintenance(', 1)[0]
        self.assertIn('SELECT EXISTS (SELECT FROM pg_ls_tmpdir())', body)
        self.assertIn('SELECT count(*) FROM pg_ls_tmpdir()', body)
        self.assertLess(body.index("label='live-spill'"), body.index("'pg_cancel_backend'"))
        self.assertLess(body.index("'pg_cancel_backend'"), body.index("label='spill-cleanup'"))
        self.assertIn("'cancel-recovery'", body)

    def test_ci_requires_normal_and_test_hook_runs(self):
        source = (ROOT / '.github/workflows/g0.yml').read_text()
        self.assertIn('Test normal grouped storage lifecycle', source)
        self.assertIn('Test grouped WAL recovery and concurrency', source)
        self.assertIn("PIN_G9_TEST_HOOKS: '1'", source)
        self.assertIn('bash -n tools/g9_qualification.sh', source)
        self.assertGreaterEqual(source.count('bash tools/g9_qualification.sh'), 2)

    def test_sql_fixture_checks_actual_reuse_and_lossification(self):
        source = (ROOT / 'tests/sql/g9_grouped.sql').read_text()
        for contract in ('Lossy Heap Blocks', 'n.ctid = o.old_tid', 'SET STORAGE PLAIN',
                         "maintenance_work_mem = '1MB'", "maintenance_work_mem = '4MB'",
                         'REINDEX INDEX', 'ROLLBACK;', 'SET pin.enable_exact_bitmap = off;'):
            self.assertIn(contract, source)


if __name__ == '__main__':
    unittest.main()
