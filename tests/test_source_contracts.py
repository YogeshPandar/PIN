"""Exercise source-policy checks with intentional one-contract mutations."""
import unittest
from tools.check_contracts import ROOT, SOURCE_PATHS, check_sources


class SourceContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = {path: (ROOT / path).read_text() for path in SOURCE_PATHS}

    def test_current_sources_match(self):
        self.assertEqual(check_sources(self.source), [])

    def assert_rejected(self, path, old, new):
        self.assertIn(old, self.source[path])
        changed = dict(self.source)
        changed[path] = changed[path].replace(old, new, 1)
        self.assertTrue(check_sources(changed))

    def test_c_field_omission_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[0], "PIN_AM_FIELD(amgettuple)", "")

    def test_rust_field_order_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[2], "IndexAmRoutine, amgettuple", "IndexAmRoutine, amgetbitmap")

    def test_capability_enablement_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[3], "amcanparallel: false", "amcanparallel: true")

    def test_scan_enablement_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[3], "amgettuple: None", "amgettuple: Some(bitmap)")

    def test_missing_callback_guard_is_detected(self):
        self.assert_rejected(
            SOURCE_PATHS[3], '#[pg_guard]\nunsafe extern "C-unwind" fn insert',
            'unsafe extern "C-unwind" fn insert',
        )

    def test_strict_handler_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[3], "CALLED ON NULL INPUT", "STRICT")

    def test_dummy_sql_argument_is_not_read_by_rust(self):
        self.assert_rejected(SOURCE_PATHS[3], "fn pin_handler()", "fn pin_handler(dummy: Internal)")

    def test_default_test_hook_enablement_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[4], '#[cfg(feature = "test-hooks")]\nmod test_hooks;', 'mod test_hooks;')

    def test_missing_hook_privilege_revocation_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[6], "FROM PUBLIC;", ";")

    def test_missing_runtime_page_check_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[5], 'c"block_size", b"8192"', 'c"block_size", b"16384"')

    def test_missing_runtime_call_is_detected(self):
        self.assert_rejected(SOURCE_PATHS[4], "compatibility::server();", "")


if __name__ == "__main__":
    unittest.main()
