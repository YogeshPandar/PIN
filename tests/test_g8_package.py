"""Package fixtures exercise tooling, not compiled extension correctness."""
import copy
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from tools.g8_package import (
    PAYLOAD, PackageError, assemble, digest, encoded, inspect_stage,
    validate_metadata, verify,
)

BUILD = {
    "schema": 1, "status": "qualification-only", "profile": "normal",
    "postgres": "18.6", "rust": "1.98.1", "pgrx": "0.19.2",
    "target": "x86_64-unknown-linux-gnu", "sql_version": "0.0.0",
    "commit": "a" * 40,
}
DEPS = {
    "packages": [
        {"id": "host", "name": "pin-pg", "version": "0.0.0"},
        {"id": "pgrx", "name": "pgrx", "version": "0.19.2"},
        {"id": "sys", "name": "pgrx-pg-sys", "version": "0.19.2"},
    ],
    "resolve": {"nodes": [{"id": "host", "features": ["pg18"]}]},
}
CONTROL = "\n".join((
    "default_version = '0.0.0'", "superuser = true", "trusted = false",
    "relocatable = false", "schema = 'pin'", "",
))


def stage(root: Path) -> Path:
    result = root / "stage"
    for name in PAYLOAD:
        path = result / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"fixture\n")
    (result / "metadata/build.json").write_bytes(encoded(BUILD))
    (result / "metadata/dependencies.json").write_bytes(encoded(DEPS))
    (result / "extension/pin.control").write_text(CONTROL)
    (result / "extension/pin--0.0.0.sql").write_bytes(b"build_profile build_revision")
    # an ELF header fixture is not evidence that the library loads.
    (result / "lib/pin.so").write_bytes(b"\x7fELF\x02\x01\x01" + b"\0" * 9 + b"\x03\x00\x3e\x00")
    return result


class PackageTests(unittest.TestCase):
    def test_roundtrip_and_deterministic_metadata(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = stage(root)
            first, second = root / "first.tgz", root / "second.tgz"
            assemble(source, first)
            for file in source.rglob("*"):
                if file.is_file():
                    file.chmod(0o600)
            assemble(source, second)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            verify(first)
            with self.assertRaises(FileExistsError):
                assemble(source, first)

    def test_deployment_metadata_rejects_gates_and_version_drift(self):
        validate_metadata(BUILD, DEPS)
        for key, value in (("profile", "test-hooks"), ("postgres", "18.5"),
                           ("commit", "unrecorded"), ("status", "released")):
            bad = dict(BUILD, **{key: value})
            with self.assertRaises(PackageError):
                validate_metadata(bad, DEPS)
        for features in (["pg18", "test-hooks"], ["pg17"], []):
            bad = copy.deepcopy(DEPS)
            bad["resolve"]["nodes"][0]["features"] = features
            with self.assertRaises(PackageError):
                validate_metadata(BUILD, bad)

    def test_bad_sql_library_control_and_inventory(self):
        changes = {
            "extension/pin--0.0.0.sql": b"build_profile build_revision g2_inject",
            "extension/pin.control": b"trusted = true\n",
            "lib/pin.so": b"not a library",
            "metadata/extra": b"unexpected",
        }
        for name, content in changes.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                source = stage(root)
                (source / name).write_bytes(content)
                with self.assertRaises(PackageError):
                    assemble(source, root / "bad.tgz")
                self.assertFalse((root / "bad.tgz").exists())

    def test_links_and_missing_payload_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            source = stage(Path(temp))
            path = source / "lib/pin.so"
            path.unlink()
            with self.assertRaises(PackageError):
                inspect_stage(source)
            path.symlink_to(source / "metadata/build.json")
            with self.assertRaises(PackageError):
                inspect_stage(source)

    def test_stream_bounds_and_short_reads(self):
        with self.assertRaises(PackageError):
            digest(io.BytesIO(b"short"), 20)
        with patch("tools.g8_package.MAX_FILE", 3):
            with self.assertRaises(PackageError):
                digest(io.BytesIO(b"data"), 4)
        with tempfile.TemporaryDirectory() as temp:
            source = stage(Path(temp))
            with patch("tools.g8_package.MAX_TOTAL", 8):
                with self.assertRaises(PackageError):
                    inspect_stage(source)

    def test_duplicate_link_traversal_and_checksum_changes_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = stage(root)
            good = root / "good.tgz"
            assemble(source, good)
            with tarfile.open(good) as archive:
                entries = [(member, archive.extractfile(member).read()) for member in archive]
            for case in ("duplicate", "link", "traversal", "checksum", "missing"):
                modified = copy.deepcopy(entries)
                if case == "duplicate":
                    modified.append(modified[1])
                elif case == "link":
                    modified[1][0].type = tarfile.SYMTYPE
                    modified[1][0].linkname = "/tmp/pin-outside"
                elif case == "traversal":
                    modified[1][0].name = "../outside"
                elif case == "checksum":
                    member, content = modified[-1]
                    modified[-1] = (member, b"X" + content[1:])
                else:
                    modified.pop()
                bad = root / f"{case}.tgz"
                with tarfile.open(bad, "w:gz") as archive:
                    for member, content in modified:
                        archive.addfile(member, io.BytesIO(content))
                with self.subTest(case=case), self.assertRaises(PackageError):
                    verify(bad)
                self.assertFalse((root / "outside").exists())

    def test_failed_postwrite_validation_removes_partial_output(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = stage(root)
            with patch("tools.g8_package.verify", side_effect=PackageError("injected")):
                with self.assertRaises(PackageError):
                    assemble(source, root / "failed.tgz")
            self.assertFalse((root / "failed.tgz").exists())


if __name__ == "__main__":
    unittest.main()
