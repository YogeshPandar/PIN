"""Source tripwires complement, but do not replace Rust and PostgreSQL tests."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class CountSourceContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.c = (ROOT / "crates/pin-pg/cshim/pin_count.c").read_text()
        cls.storage = (ROOT / "crates/pin-pg/cshim/pin_storage.c").read_text()
        cls.core = (ROOT / "crates/pin-core/src/mutable/count.rs").read_text()
        cls.rust = (ROOT / "crates/pin-pg/src/count.rs").read_text()

    def test_both_count_switches_default_off_and_superuser_settable(self):
        for name in ("pin_enable_count", "pin_enable_count_vm"):
            self.assertIn(f"static bool {name} = false;", self.c)
            self.assertIn(f"&{name}, false, PGC_SUSET", self.c)

    def test_no_synchronous_or_index_only_capability_is_claimed(self):
        am = (ROOT / "crates/pin-pg/src/am.rs").read_text()
        self.assertIn("amgettuple: None", am)
        self.assertIn("amcanreturn: None", am)

    def test_owner_copy_keeps_only_a_pin_after_validation(self):
        body = self.storage.split("pin_storage_owner_read(", 1)[1].split(
            "\nvoid\npin_storage_commit", 1
        )[0]
        self.assertLess(body.index("LockBuffer(*held, BUFFER_LOCK_SHARE)"), body.index("memcpy("))
        self.assertLess(body.index("memcpy("), body.index("LockBuffer(*held, BUFFER_LOCK_UNLOCK)"))
        self.assertNotIn("ReleaseBuffer(*held)", body)

    def test_vm_status_requires_owner_pin_and_fresh_core_read(self):
        body = self.c.split("pin_count_all_visible(void *context", 1)[1].split(
            "\nbool\npin_count_fetch", 1
        )[0]
        self.assertIn("BufferIsValid(state->owner_buffer)", body)
        self.assertIn("pin_enable_count_vm", body)
        self.assertEqual(body.count("visibilitymap_get_status("), 1)
        self.assertNotIn("cached", body.lower())

    def test_heap_fetch_requires_same_owner_pin(self):
        body = self.c.split("pin_count_fetch(void *context", 1)[1].split(
            "\nvoid\npin_count_clear", 1
        )[0]
        self.assertIn("!BufferIsValid(state->owner_buffer)", body)
        self.assertIn("table_index_fetch_tuple", body)
        self.assertIn("&visible", body)
        self.assertLess(self.rust.index("pin_count_fetch(context"), self.rust.index(
            "pin_count_owner_unlock(context)"
        ))

    def test_owner_removal_uses_cleanup_permission_before_wal(self):
        body = self.storage.split("pin_storage_remove_owners(", 1)[1].split(
            "\nvoid\npin_storage_interrupt", 1
        )[0]
        self.assertLess(
            body.index("LockBufferForCleanup(buffer)"),
            body.index("GenericXLogStart(index)"),
        )
        vacuum = (ROOT / "crates/pin-core/src/mutable/vacuum.rs").read_text()
        self.assertIn("store.remove_owners(&page)?;", vacuum)
        self.assertNotIn("store.commit(&[&page])?;", vacuum)

    def test_unknown_adapters_cannot_bypass_cleanup_interlock(self):
        module = (ROOT / "crates/pin-core/src/mutable/mod.rs").read_text()
        body = module.split("fn remove_owners(", 1)[1].split("\n    }", 1)[0]
        self.assertIn("Err(Error::InvalidState)", body)
        self.assertNotIn("self.commit", body)

    def test_runtime_fallback_retains_real_aggregate_child(self):
        self.assertIn("memcpy(saved, fallback, sizeof(AggPath))", self.c)
        self.assertLess(self.c.index("memcpy(saved"), self.c.index("add_path(output"))
        self.assertIn("path->custom_paths = list_make1(saved)", self.c)
        self.assertIn("ExecProcNode(state->fallback)", self.c)
        self.assertIn("ExecReScan(state->fallback)", self.c)
        self.assertIn("ExecEndNode(state->fallback)", self.c)
        self.assertIn("IsolationIsSerializable()", self.c)
        self.assertIn("IsMVCCSnapshot(estate->es_snapshot)", self.c)

    def test_count_candidate_stream_is_bounded_and_safe(self):
        for forbidden in ("Vec<", "HashSet", "BTreeSet", "unsafe"):
            self.assertNotIn(forbidden, self.core)
        self.assertIn("sealed_term", self.core)
        self.assertIn("checked_add(1)", self.core)

    def test_count_pause_points_cover_pin_and_visibility_windows(self):
        module = (ROOT / "crates/pin-core/src/mutable/mod.rs").read_text()
        hooks = (ROOT / "crates/pin-pg/src/test_hooks.rs").read_text()
        for name in ("CountOwnerPinned", "CountBeforeVisibility", "CountAfterVisibility"):
            self.assertIn(name, module)
            self.assertIn(name, self.rust)
        self.assertIn("(1..=15)", hooks)


if __name__ == "__main__":
    unittest.main()
