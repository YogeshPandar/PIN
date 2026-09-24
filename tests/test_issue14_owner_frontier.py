"""Owner-frontier activation and evidence guards; not live PostgreSQL evidence."""
from contextlib import contextmanager
import importlib
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
from tools import frontier_options
from tools import g9_profile as paired
from tools import owner_frontier_options as gates
from tools import g9_qualification as qualification

cpu = importlib.import_module('g9_cpu_profile')
bench = importlib.import_module('issue14_frontier_bench')


@contextmanager
def connection_stub(owner_setting, anchor_setting='off'):
    driver = MagicMock()
    conn = driver.connect.return_value
    cur = conn.cursor.return_value
    cur.__enter__.return_value = cur

    def execute(sql, *_args, **_kwargs):
        if sql == frontier_options.SETTING_SQL:
            cur.fetchone.return_value = None if anchor_setting is None else (anchor_setting,)
        elif sql == gates.SETTING_SQL:
            cur.fetchone.return_value = None if owner_setting is None else (owner_setting,)

    cur.execute.side_effect = execute
    with patch.dict(sys.modules, {'psycopg2': driver}):
        yield conn, cur


class OwnerFrontierActivationTests(unittest.TestCase):
    def test_gate_defaults_off_without_mutating_shared_settings(self):
        original = {'work_mem': '64MB'}
        self.assertEqual(gates.options(original, False)[gates.NAME], 'off')
        self.assertEqual(gates.options(original, True)[gates.NAME], 'on')
        self.assertEqual(original, {'work_mem': '64MB'})

    def test_missing_registered_setting_is_only_valid_for_disabled_controls(self):
        self.assertIsNone(gates.require_setting(None, False))
        self.assertIsNone(gates.require_setting('absent', False))
        for enabled, expected in ((False, 'off'), (True, 'on')):
            self.assertEqual(gates.require_setting(expected, enabled), expected)
            with self.assertRaises(ValueError):
                gates.require_setting(None, enabled=True)

    def test_psycopg_backends_apply_and_verify_the_requested_gate(self):
        for factory in (bench.connection, cpu.connect):
            for enabled in (False, True):
                state = 'on' if enabled else 'off'
                with connection_stub(state) as (conn, cur):
                    result = factory('grouped', owner_frontier=enabled)
                    self.assertIs(result[0] if isinstance(result, tuple) else result, conn)
                    statements = [call.args[0] for call in cur.execute.call_args_list]
                    self.assertIn(gates.SETTING_SQL, statements)
                    self.assertTrue(any(gates.NAME in sql and state in sql for sql in statements))
            with connection_stub(None) as (conn, _):
                with self.assertRaises(ValueError):
                    factory('grouped', owner_frontier=True)
                conn.close.assert_called_once()

    def test_psql_and_child_processes_retain_activation(self):
        with tempfile.TemporaryDirectory() as temp:
            for enabled in (False, True):
                log = Path(temp) / f'paired-{enabled}'
                with patch.object(paired.subprocess, 'Popen') as popen:
                    session = paired.Session(Path('/fixture/psql'), log, paired.MODES[0],
                                             owner_frontier=enabled)
                    try:
                        options = popen.call_args.kwargs['env']['PGOPTIONS']
                        self.assertIn(f'{gates.NAME}={"on" if enabled else "off"}', options)
                    finally:
                        session.close()
                with patch.object(bench.subprocess, 'run') as run:
                    bench.child(Path(temp), f'child-{enabled}', ['python3', 'profile.py'],
                                owner_frontier=enabled)
                    argv = run.call_args.args[0]
                    self.assertEqual('--owner-frontier' in argv, enabled)
                    options = run.call_args.kwargs['env']['PGOPTIONS']
                    self.assertIn(f'{gates.NAME}={"on" if enabled else "off"}', options)

    def test_source_keeps_default_off_and_fail_before_emit_fallback(self):
        grouped = (ROOT / 'crates/pin-pg/src/grouped.rs').read_text()
        frontier = (ROOT / 'crates/pin-core/src/mutable/grouped/frontier.rs').read_text()
        core = (ROOT / 'crates/pin-core/src/mutable/mod.rs').read_text()
        self.assertIn('pin.enable_owner_frontier', grouped)
        self.assertIn(
            'ENABLE_OWNER_FRONTIER: GucSetting<bool> = GucSetting::<bool>::new(false)',
            grouped,
        )
        self.assertIn('OwnerFrontierScan = 41', core)
        self.assertIn('scan_owner_frontier', frontier)
        self.assertLess(frontier.index('let mut output'), frontier.index('for root in output'))
        self.assertIn('OWNER_FRONTIER_MIN_SPAN: u64 = 512', frontier)

    def test_native_matrix_activates_read_path_and_recovery_checks(self):
        workflow = (ROOT / '.github/workflows/g0.yml').read_text()
        driver = (ROOT / 'tools/g9_qualification.py').read_text()
        shell = (ROOT / 'tools/g9_qualification.sh').read_text()
        self.assertEqual(workflow.count("PIN_G9_OWNER_FRONTIER: '1'"), 2)
        for contract in ('pin.g2_inject(41, 1, false)',
                         'writer-during-owner-frontier-read',
                         'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_owner_frontier',
                         'cluster.restart(immediate=True)',
                         "owner frontier must default off"):
            self.assertIn(contract, driver)
        self.assertIn('PIN_G9_OWNER_FRONTIER', shell)
        with tempfile.TemporaryDirectory() as temp:
            cluster = qualification.Cluster(MagicMock(artifacts=Path(temp),
                                                      owner_frontier='1', anchors='0'))
            self.assertTrue(cluster.owner_frontier)
            self.assertIn(
                'SET pin.enable_owner_frontier = on;', cluster.settings('SELECT 1;')
            )


if __name__ == '__main__':
    unittest.main()
