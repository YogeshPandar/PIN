"""Native-driver drift checks and CPU-host setup mocks, not native SQL results."""
import ast
from pathlib import Path
from unittest.mock import patch
import unittest

from tools import g9_count_qualification as qualification

ROOT = Path(__file__).resolve().parents[1]


class QualificationContracts(unittest.TestCase):
    def test_writer_wait_observes_actual_interlock(self):
        class Cluster:
            def __init__(self):
                self.statements = []
            def run(self, sql, **kwargs):
                self.statements.append(sql)
                return 't'
        cluster = Cluster()
        with patch.object(qualification.time, 'monotonic', return_value=1):
            qualification.wait_writer(cluster, 'reader-writer-test')
        self.assertEqual(len(cluster.statements), 1)
        for contract in ("l.page=0", "l.mode='ExclusiveLock'", 'NOT l.granted'):
            self.assertIn(contract, cluster.statements[0])

    def test_native_matrix_keeps_normal_and_hooked_runs(self):
        workflow = (ROOT / '.github/workflows/g0.yml').read_text()
        self.assertGreaterEqual(workflow.count('bash tools/g9_count_qualification.sh'), 2)
        source = (ROOT / 'tools/g9_count_qualification.py').read_text()
        ast.parse(source)
        for contract in ('(42, 1), (15, 2)', 'pg_cancel_backend', 'pg_terminate_backend',
                         'BEGIN ISOLATION LEVEL REPEATABLE READ', 'IsOLATION LEVEL SERIALIZABLE'.upper(),
                         'n_tup_hot_upd', 'n.ctid=o.old_tid', 'json_build_array(id,ctid::text)',
                         'gc_witness', 'Grouped Count Runs', 'ROW LEVEL SECURITY',
                         "'64kB'", 'writer_contention(cluster)'):
            # the SQL string embeds an escaped quote around the memory setting.
            if contract == "'64kB'":
                self.assertIn('64kB', source)
            else:
                self.assertIn(contract, source)

    def test_boundary_checks_before_and_after_visibility_are_hooked(self):
        source = (ROOT / 'crates/pin-pg/src/grouped_count.rs').read_text()
        before = source.index('Stage::CountBeforeVisibility')
        probe = source.index('native::call(|| pin_count_all_visible')
        after = source.index('Stage::CountAfterVisibility')
        self.assertLess(before, probe)
        self.assertLess(probe, after)
        self.assertIn('#[cfg(feature = "test-hooks")]', source[:before])

    def test_benchmark_refuses_placeholders_and_dirty_provenance(self):
        source = (ROOT / 'tools/g9_count_bench.py').read_text()
        for contract in ('name not in environment', 'source-SHA256.json', 'source-head.txt',
                         'source-status.txt', 'source-diff.patch', 'source provenance requires',
                         'harness worktree and binary revisions differ'):
            self.assertIn(contract, source)


if __name__ == '__main__':
    unittest.main()
