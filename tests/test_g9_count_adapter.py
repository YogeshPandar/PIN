"""Compile the actual new C bridge bodies with host doubles; not native PG proof."""
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
FUNCTIONS = ('pin_count_generation_try_lock', 'pin_count_generation_unlock',
             'pin_count_fetch_visible', 'pin_count_all_visible')


def function(source: str, name: str) -> str:
    match = re.search(r'\n(?:bool|void)\n' + re.escape(name) + r'\(', source)
    if match is None:
        raise ValueError(f'missing function {name}')
    start = source.index('{', match.start())
    depth = 1
    end = start + 1
    while depth:
        if end >= len(source):
            raise ValueError(f'unclosed function {name}')
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[match.start():end] + '\n'


class CountContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.c = (ROOT / 'crates/pin-pg/cshim/pin_count.c').read_text()
        cls.rust = (ROOT / 'crates/pin-pg/src/grouped_count.rs').read_text()

    def test_guard_precedes_reads_and_normal_errors_release_before_propagation(self):
        body = self.rust.split('storage::with_reader(index, |store|', 1)[1]
        self.assertLess(body.index('pin_count_generation_try_lock'), body.index('grouped::scan_exact'))
        self.assertLess(body.index('pin_count_generation_unlock'), body.index('\n            scanned'))
        self.assertLess(body.index('let Some(candidates)'), body.index('result.write'))
        self.assertIn('return Ok(None)', body)
        self.assertIn('memory_bytes.min(matching::QUERY_MEMORY)', body)

    def test_c_and_rust_counter_extents_match(self):
        old = (ROOT / 'crates/pin-pg/src/count.rs').read_text()
        c_count = int(re.search(r'#define PIN_COUNT_STATS (\d+)', self.c)[1])
        rust_count = int(re.search(r'const COUNTERS: usize = (\d+)', old)[1])
        self.assertEqual(c_count, rust_count)
        self.assertEqual(c_count, 18)
        for label in ('Grouped Count Runs', 'Index Page Reads', 'Index Payload Bytes'):
            self.assertIn(label, self.c)

    def test_new_gate_is_off_and_cached_compound_plans_can_fall_back(self):
        self.assertIn('GucSetting::<bool>::new(false)', self.rust)
        self.assertIn('GucContext::Suset', self.rust)
        body = self.c.split('pin_count_next(CustomScanState *node)', 1)[1]
        self.assertLess(body.index('pin_count_grouped_enabled'), body.index('pin_count_parallel_run'))
        self.assertIn('grouped count disabled at execution', body)
        self.assertIn('grouped snapshot, budget or writer contention', body)
        self.assertIn('ExecProcNode(state->fallback)', body)

    def test_protected_heap_bridge_does_not_retokenize_or_follow_raw_heap_tid(self):
        body = function(self.c, 'pin_count_fetch_visible')
        for required in ('state->generation_locked', 'IsMVCCSnapshot', 'CHECK_FOR_INTERRUPTS',
                         'table_index_fetch_tuple', '&visible', 'if (again)'):
            self.assertIn(required, body)
        for forbidden in ('slot_getattr', 'heap_fetch', 'table_tuple_fetch_row_version'):
            self.assertNotIn(forbidden, body)

    @unittest.skipUnless(shutil.which('cc'), 'C compiler unavailable')
    def test_real_bridge_debug_and_ubsan(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / 'g9_count_functions.inc').write_text(
                ''.join(function(self.c, name) for name in FUNCTIONS))
            for flags in (['-O0'], ['-O2', '-fsanitize=undefined,bounds',
                                    '-fno-sanitize-recover=all']):
                binary = directory / 'visibility'
                command = [shutil.which('cc'), '-std=c11', '-Wall', '-Wextra', '-Werror',
                           '-fno-strict-aliasing', '-fwrapv', *flags, '-I', str(directory),
                           str(ROOT / 'tests/support/g9_count_visibility.c'), '-o', str(binary)]
                built = subprocess.run(command, capture_output=True, text=True, timeout=30)
                self.assertEqual(built.returncode, 0, built.stderr)
                run = subprocess.run([str(binary)], capture_output=True, text=True, timeout=10)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertIn('13 rejected calls', run.stdout)


if __name__ == '__main__':
    unittest.main()
