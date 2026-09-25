"""Activation, provenance and host-driver guards; not PostgreSQL execution evidence."""
import argparse
from contextlib import contextmanager
import importlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
from tools import frontier_options as gates
from tools import owner_frontier_options as owner_gates
from tools import g9_profile as paired
from tools import g9_qualification as qualification

cpu = importlib.import_module('g9_cpu_profile')
bench = importlib.import_module('issue14_frontier_bench')


@contextmanager
def connection_stub(setting, owner_setting='off'):
    driver = MagicMock()
    conn = driver.connect.return_value
    cur = conn.cursor.return_value
    cur.__enter__.return_value = cur

    def execute(sql, *_args, **_kwargs):
        if sql == gates.SETTING_SQL:
            cur.fetchone.return_value = None if setting is None else (setting,)
        elif sql == owner_gates.SETTING_SQL:
            cur.fetchone.return_value = None if owner_setting is None else (owner_setting,)

    cur.execute.side_effect = execute
    with patch.dict(sys.modules, {'psycopg2': driver}):
        yield conn, cur


class AnchorActivationTests(unittest.TestCase):
    def test_gate_defaults_off_without_mutating_shared_settings(self):
        original = {'work_mem': '64MB'}
        self.assertEqual(gates.options(original, False)[gates.NAME], 'off')
        self.assertEqual(gates.options(original, True)[gates.NAME], 'on')
        self.assertEqual(original, {'work_mem': '64MB'})

    def test_placeholder_is_not_proof_of_registered_activation(self):
        self.assertIsNone(gates.require_setting(None, False))
        self.assertIsNone(gates.require_setting('absent', False))
        for flag, expected in ((False, 'off'), (True, 'on')):
            self.assertEqual(gates.require_setting(expected, flag), expected)
            for actual in ('on', 'off', 'absent', None, 'true', ''):
                if actual != expected and not (not flag and actual in ('absent', None)):
                    with self.assertRaises(ValueError):
                        gates.require_setting(actual, flag)

    def test_every_backend_applies_and_verifies_the_requested_gate(self):
        for factory in (bench.connection, cpu.connect):
            for enabled in (False, True):
                for mode in ('gin', 'pin_legacy', 'grouped'):
                    state = 'on' if enabled else 'off'
                    with connection_stub(state) as (conn, cur):
                        result = factory(mode, anchors=enabled)
                        self.assertIs(result[0] if isinstance(result, tuple) else result, conn)
                        statements = [call.args[0] for call in cur.execute.call_args_list]
                        self.assertIn(gates.SETTING_SQL, statements)
                        self.assertTrue(any(gates.NAME in sql and state in sql for sql in statements))
                        conn.close.assert_not_called()
            with connection_stub(None) as (conn, _):
                with self.assertRaises(ValueError):
                    factory('gin', anchors=True)
                conn.close.assert_called_once()

    def test_paired_psql_does_not_lose_gate_when_replacing_pgoptions(self):
        with tempfile.TemporaryDirectory() as temp:
            for enabled in (False, True):
                log = Path(temp) / str(enabled)
                with patch.object(paired.subprocess, 'Popen') as popen:
                    session = paired.Session(Path('/fixture/psql'), log, paired.MODES[0],
                                             frontier_anchors=enabled)
                    try:
                        actual = popen.call_args.kwargs['env']['PGOPTIONS']
                        self.assertIn(f'{gates.NAME}={"on" if enabled else "off"}', actual)
                        self.assertIn(f'{owner_gates.NAME}=off', actual)
                    finally:
                        session.close()

    def test_child_profiler_gets_both_flag_and_session_options(self):
        with tempfile.TemporaryDirectory() as temp:
            for enabled in (False, True):
                with patch.object(bench.subprocess, 'run') as run:
                    bench.child(Path(temp), str(enabled), ['python3', 'profile.py'], anchors=enabled)
                    argv = run.call_args.args[0]
                    self.assertEqual('--frontier-anchors' in argv, enabled)
                    options = run.call_args.kwargs['env']['PGOPTIONS']
                    self.assertIn(f'{gates.NAME}={"on" if enabled else "off"}', options)
                    self.assertIn(f'{owner_gates.NAME}=off', options)

    def test_cpu_cases_cover_the_same_boolean_and_fallback_classes(self):
        self.assertEqual(cpu.CASES, paired.CASES)
        self.assertEqual(set(cpu.CASES), {'common', 'rare', 'and', 'selective_and',
                                         'or', 'not', 'phrase', 'prefix', 'absent'})
        source = (ROOT / 'tools/issue14_frontier_bench.py').read_text()
        self.assertIn('cpu_cases = args.cases', source)
        self.assertIn("'--profile-cases', *cpu_cases", source)
        self.assertIn('args.concurrent_related', source)
        self.assertIn('related_writes', source)

    def test_optional_updates_keep_individual_and_lifecycle_write_costs(self):
        with tempfile.TemporaryDirectory() as temp:
            for updated in (0, 7):
                args = argparse.Namespace(output=Path(temp), write_batches=1,
                                          write_rows=10, write_update_rows=updated)
                cur = MagicMock()
                cur.fetchone.side_effect = [(100, 200), (100, 200), (0,)]
                phase = {'backend': {'on_cpu_ns': 11}, 'cluster_wal_bytes': 13}
                with patch.object(bench, 'measured', return_value=phase) as measure:
                    bench.writes(args, cur)
                statements = [call.args[1] for call in measure.call_args_list]
                updates = [sql for sql in statements if sql.startswith('UPDATE ')]
                self.assertEqual(len(updates), 2 if updated else 0)
                if updated:
                    self.assertTrue(all('WHERE id BETWEEN 1 AND 7' in sql for sql in updates))
                result = json.loads((Path(temp) / 'write-maintenance.json').read_text())
                self.assertEqual(len(result), 2)
                for item in result:
                    self.assertEqual(item['updated_rows'], updated)
                    self.assertEqual(item['update'] is None, updated == 0)
                    self.assertEqual(item['lifecycle_backend_cpu_ns'], 33 if updated else 22)
                    self.assertEqual(item['lifecycle_cluster_wal_bytes'], 39 if updated else 26)

    def test_cpu_sampler_rejects_pid_reuse_and_counter_reversal(self):
        before = {'start_ticks': 5, 'on_cpu_ns': 100, 'read_bytes': 7}
        self.assertEqual(cpu.delta(before, dict(before, on_cpu_ns=160)),
                         {'on_cpu_ns': 60, 'read_bytes': 0})
        for after in (dict(before, start_ticks=6), dict(before, read_bytes=6)):
            with self.assertRaises(RuntimeError):
                cpu.delta(before, after)


class AnchorQualificationTests(unittest.TestCase):
    def test_foreground_and_concurrent_sql_use_the_same_activation(self):
        with tempfile.TemporaryDirectory() as temp:
            cluster = qualification.Cluster(argparse.Namespace(
                psql='/fixture/psql', artifacts=Path(temp), anchors='1'))
            sql = 'SET pin.enable_frontier_anchors = off;\nVACUUM g9_anchor;'
            with patch.object(qualification.subprocess, 'run') as run:
                run.return_value = subprocess.CompletedProcess([], 0, '', '')
                cluster.run(sql)
                written = run.call_args.kwargs['input']
                self.assertTrue(written.startswith('SET pin.enable_frontier_anchors = on;'))
                self.assertIn(sql, written)
            with patch.object(qualification.subprocess, 'Popen'):
                cluster.start(sql, 'fixture')
            self.assertEqual((Path(temp) / '002-fixture.sql').read_text(), written)

    def test_seek_and_invalidation_probes_are_distinct_and_privileged(self):
        core = (ROOT / 'crates/pin-core/src/mutable/mod.rs').read_text()
        frontier = (ROOT / 'crates/pin-core/src/mutable/grouped/frontier.rs').read_text()
        hooks = (ROOT / 'crates/pin-pg/src/test_hooks.rs').read_text()
        self.assertIn('FrontierInvalidated = 39', core)
        self.assertIn('FrontierSeek = 40', core)
        self.assertIn('OwnerFrontierScan = 41', core)
        self.assertEqual(frontier.count('store.event(Stage::FrontierSeek)?'), 2)
        self.assertIn('!(32..=41).contains(&stage)', hooks)
        self.assertIn('if !unsafe { pg_sys::superuser() }', hooks)

    def test_anchored_ci_covers_error_replay_cancellation_and_concurrent_append(self):
        workflow = (ROOT / '.github/workflows/g0.yml').read_text()
        driver = (ROOT / 'tools/g9_qualification.py').read_text()
        self.assertEqual(workflow.count("PIN_G9_ANCHORS: '1'"), 2)
        self.assertIn('(*STAGES, 39) if cluster.anchors else STAGES', driver)
        for probe in ('prove-frontier-seek', 'prove-anchor-fallback',
                      'prove-invalidated-fallback', 'related-writer-during-anchor-read',
                      'g9-anchor-cancel', 'anchor-permissions'):
            self.assertIn(probe, driver)
        seek = driver.split('def anchor_seek_qualification(', 1)[1].split('def permissions(', 1)[0]
        self.assertIn('SET pin.enable_owner_frontier = off;', seek)
        self.assertLess(seek.index("wait_lock('g9-anchor-reader', False)"),
                        seek.index('related-writer-during-anchor-read'))
        self.assertLess(seek.index('related-writer-during-anchor-read'),
                        seek.index("wait_structure_lock('g9-anchor-maintenance')"))

    def test_isolated_runner_rejects_provenance_overrides_before_host_access(self):
        runner = ROOT / 'tools/issue14_isolated_run.sh'
        for option in ('--revision=other', '--output=other', '--bindir=other',
                       '--frontier-anchors', '--owner-frontier'):
            result = subprocess.run(
                ['bash', str(runner), 'a' * 40, '/unused-output', '0', option],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn('controlled by this runner', result.stderr)

    def test_isolated_runner_separates_builds_and_finishes_logs_before_hashing(self):
        source = (ROOT / 'tools/issue14_isolated_run.sh').read_text()
        for contract in ('mkdir "$output/target" "$output/build"',
                         'CARGO_TARGET_DIR="$output/target"',
                         'CARGO_BUILD_BUILD_DIR="$output/build"',
                         "RUSTC_WRAPPER='' RUSTC_WORKSPACE_WRAPPER=''",
                         'cargo build --locked --release -p pin-pg',
                         'cmp "$output/source/Cargo.lock"',
                         '[[ ! -e $lib ]]', 'sha256sum "$lib"',
                         'git -C "$output/source" archive', 'mktemp -d /tmp/pin-g9-run-',
                         'exit-status.txt'):
            self.assertIn(contract, source)
        self.assertNotIn('>(tee', source)
        cleanup = source.split('cleanup() {', 1)[1].split('trap cleanup EXIT', 1)[0]
        self.assertLess(cleanup.index('exec 1>&3 2>&4'), cleanup.index('xargs -0'))
        self.assertIn('allow_abbrev=False', (ROOT / 'tools/issue14_frontier_bench.py').read_text())

    def test_sql_long_history_and_independent_identity_oracles_are_required(self):
        source = (ROOT / 'tests/sql/issue14_anchors.sql').read_text()
        for contract in ('generate_series(1, 16384)', 'pg_temp.anchor_check()',
                         'expected bigint[]', 'actual IS DISTINCT FROM expected',
                         "SET LOCAL enable_seqscan = on", "'off', 'on'", 'gin(to_tsvector',
                         'Rows Removed by Index Recheck', 'ROLLBACK;',
                         'UPDATE g9_anchor SET body', 'DELETE FROM g9_anchor',
                         'VACUUM (INDEX_CLEANUP ON, PARALLEL 0) g9_anchor'):
            self.assertIn(contract, source)
        self.assertIn('tests/sql/g9_grouped.sql',
                      (ROOT / 'tools/g9_qualification.py').read_text())


if __name__ == '__main__':
    unittest.main()
