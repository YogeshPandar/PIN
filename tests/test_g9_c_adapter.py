"""Compile the real C bridge against test doubles, not a PostgreSQL runtime."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(shutil.which('cc'), 'C compiler is unavailable')
class CAdapterTests(unittest.TestCase):
    def test_marshalling_in_debug_and_sanitized_optimized_builds(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            for name in (
                'postgres.h', 'miscadmin.h', 'varatt.h', 'utils/tuplesort.h',
                'catalog/pg_operator_d.h', 'catalog/pg_type_d.h', 'postmaster/autovacuum.h',
            ):
                target = directory / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text('#include "g9_pg_mock.h"\n')
            for flags in (['-O0'], ['-O2', '-fsanitize=undefined,bounds', '-fno-sanitize-recover=all']):
                binary = directory / 'adapter'
                command = [
                    shutil.which('cc'), '-std=c11', '-Wall', '-Wextra', '-Werror',
                    '-fno-strict-aliasing', '-fwrapv', '-fexcess-precision=standard', *flags,
                    '-I', str(directory), '-I', str(ROOT / 'tests/support'),
                    '-I', str(ROOT / 'crates/pin-pg/cshim'),
                    str(ROOT / 'crates/pin-pg/cshim/pin_grouped.c'),
                    str(ROOT / 'tests/support/g9_sort_adapter.c'), '-o', str(binary),
                ]
                built = subprocess.run(command, capture_output=True, text=True, timeout=30, check=False)
                self.assertEqual(built.returncode, 0, built.stderr)
                run = subprocess.run([str(binary)], capture_output=True, text=True, timeout=10, check=False)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertIn('300 records, 12 rejected calls', run.stdout)


if __name__ == '__main__':
    unittest.main()
