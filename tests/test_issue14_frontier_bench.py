"""Guard and measurement contracts for the disposable frontier driver."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

TOOLS = Path(__file__).resolve().parents[1] / 'tools'
sys.path.insert(0, str(TOOLS))
SPEC = importlib.util.spec_from_file_location('frontier_bench', TOOLS / 'issue14_frontier_bench.py')
BENCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCH)


class FrontierBenchTests(unittest.TestCase):
    def test_delta_growth_is_bounded_and_never_rewinds(self):
        BENCH.validate_deltas([0, 1, 1000, 10000])
        for values in ([], [1], [0, 0], [0, -1], [0, 100, 1], [0, 1_000_001]):
            with self.assertRaises(ValueError):
                BENCH.validate_deltas(values)

    def test_host_requires_exact_binary_and_durable_pinned_server(self):
        revision = 'a' * 40
        settings = dict(server_version_num='180006', block_size='8192', fsync='on',
                        full_page_writes='on', synchronous_commit='on')
        with tempfile.TemporaryDirectory(prefix='pin-g9-', dir='/tmp') as name:
            path = Path(name)
            (path / 'postmaster.pid').write_text('123\n')
            BENCH.validate_host(settings, revision, revision, path)
            for field, value in [('server_version_num', '180005'), ('block_size', '4096'),
                                 ('fsync', 'off'), ('full_page_writes', 'off'), ('synchronous_commit', 'off')]:
                with self.assertRaises(ValueError):
                    BENCH.validate_host(dict(settings, **{field: value}), revision, revision, path)
            with self.assertRaises(ValueError):
                BENCH.validate_host(settings, 'b' * 40, revision, path)
            with self.assertRaises(ValueError):
                BENCH.validate_host(settings, revision, 'main', path)
            (path / 'postmaster.pid').unlink()
            with self.assertRaises(ValueError):
                BENCH.validate_host(settings, revision, revision, path)

    def test_pid_reuse_and_counter_reversal_fail_closed(self):
        before = {'start_ticks': 12, 'on_cpu_ns': 100, 'read_bytes': 10}
        self.assertEqual(BENCH.difference(before, dict(before, on_cpu_ns=145)),
                         {'on_cpu_ns': 45, 'read_bytes': 0})
        for after in (dict(before, start_ticks=13), dict(before, read_bytes=9)):
            with self.assertRaises(RuntimeError):
                BENCH.difference(before, after)

    def test_disabled_cpu_accounting_is_not_a_zero_cpu_result(self):
        for schedstat in ('0 0 0', '1 2'):
            with patch.object(Path, 'read_text', side_effect=['123 (postgres) S', schedstat]):
                with self.assertRaises(RuntimeError):
                    BENCH.proc(123)

    def test_engine_predicates_keep_explicit_semantics(self):
        self.assertIn('OPERATOR(pin.@@@)', BENCH.predicate('selective_and', 'pin_grouped_enabled'))
        self.assertIn("'alpha & rareplanet'", BENCH.predicate('selective_and', 'gin'))
        self.assertIn("'beta <-> gamma'", BENCH.predicate('phrase', 'gin'))

    def test_manifest_writer_does_not_accept_nonfinite_measurements(self):
        with tempfile.TemporaryDirectory() as name:
            path = Path(name) / 'results.json'
            BENCH.save(path, {'status': 'not_measured', 'cpu_ns': None})
            self.assertTrue(path.exists())
            with self.assertRaises(ValueError):
                BENCH.save(path, {'cpu_ns': float('nan')})


if __name__ == '__main__':
    unittest.main()
