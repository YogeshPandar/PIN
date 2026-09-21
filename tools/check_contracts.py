#!/usr/bin/env python3
"""Check G0 source contracts. This is drift detection, not a Rust/C compiler."""
from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE_PATHS = (
    "crates/pin-pg/cshim/am_fields.def",
    "crates/pin-pg/cshim/pin_abi.c",
    "crates/pin-pg/src/abi.rs",
    "crates/pin-pg/src/am.rs",
    "crates/pin-pg/src/lib.rs",
    "crates/pin-pg/src/compatibility.rs",
    "crates/pin-pg/src/test_hooks.rs",
    "crates/pin-core/src/lib.rs",
)


def check_sources(source: dict[str, str]) -> list[str]:
    """Return violations in the supplied complete set of policy source files."""
    errors: list[str] = []
    c_fields = re.findall(r"^PIN_AM_FIELD\((\w+)\)$", source[SOURCE_PATHS[0]], re.M)
    rust_fields = re.findall(
        r"offset_of!\(pg_sys::IndexAmRoutine,\s*(\w+)\)", source[SOURCE_PATHS[2]]
    )
    expected = ["type_" if field == "type" else field for field in c_fields]
    if len(c_fields) != 51 or len(set(c_fields)) != 51 or rust_fields != expected:
        errors.append("C and Rust must enumerate the same 51 unique AM fields")
    am = source[SOURCE_PATHS[3]]
    initializer = re.search(r"let routine = pg_sys::IndexAmRoutine \{(.*?)\n    \};", am, re.S)
    if initializer is None:
        errors.append("AM initializer must remain explicit and exhaustive")
    else:
        entries = re.findall(r"^\s*(\w+):\s*([^\n]+),$", initializer[1], re.M)
        if [field for field, _ in entries] != expected:
            errors.append("AM initializer must match the independently enumerated field order")
        values = dict(entries)
        false_flags = expected[4:23]
        implemented_true = {"amcanparallel", "amcanbuildparallel"}
        if any(
            values.get(flag) != ("true" if flag in implemented_true else "false")
            for flag in false_flags
        ):
            errors.append("AM boolean capabilities must match implemented gates")
        if values.get("amcanreturn") != "None":
            errors.append("amcanreturn must stay disabled before index-only support")
        expected_scan = {
            "amgettuple": "Some(native::pin_scan_gettuple)",
            "amestimateparallelscan": "Some(native::pin_scan_estimate_parallel)",
            "aminitparallelscan": "Some(native::pin_scan_init_parallel)",
            "amparallelrescan": "Some(parallel_rescan)",
        }
        for field, value in expected_scan.items():
            if values.get(field) != value:
                errors.append(f"{field} must match the native parallel scan boundary")
        if values.get("amgetbitmap") != "Some(bitmap)" or values.get("aminsertcleanup") != "Some(insert_cleanup)":
            errors.append("G2 requires guarded bitmap and insert-cleanup callbacks")
        for field, value in entries:
            callback = re.fullmatch(r"Some\((\w+)\)", value)
            if callback and not re.search(
                r'#\[pg_guard\]\s*unsafe extern "C-unwind" fn ' + callback[1] + r"\(", am
            ):
                errors.append(f"{field} lacks its C-unwind PostgreSQL entry guard")
    sql = re.search(r'#\[pg_extern\(sql = r#"(.*?)"#\)\]', am, re.S)
    if sql is None or "RETURNS index_am_handler" not in sql[1] or (
        "CALLED ON NULL INPUT" not in sql[1] or re.search(r"\bSTRICT\b", sql[1])
    ):
        errors.append("AM handler must return index_am_handler and accept null input")
    if not re.search(r"fn pin_handler\(\) -> Internal", am):
        errors.append("core supplies zero handler arguments; do not extract the SQL dummy")
    if "pg_sys::palloc(" not in am or "node.write(routine)" not in am:
        errors.append("AM node must be fully initialized in PostgreSQL-owned storage")
    if not re.search(
        r'#\[cfg\(feature = "test-hooks"\)\]\s*mod test_hooks;', source[SOURCE_PATHS[4]]
    ):
        errors.append("test hooks must be excluded from the default build")
    hooks = source[SOURCE_PATHS[6]]
    if hooks.count("REVOKE ALL ON FUNCTION") != hooks.count("FROM PUBLIC;") or "FROM PUBLIC;" not in hooks:
        errors.append("test hook execution privileges must be revoked from PUBLIC")
    hook_names = re.findall(r"^fn (g0_\w+)\(", hooks, re.M)
    for name in hook_names:
        if f"pin.{name}()" not in hooks:
            errors.append(f"test hook {name} is absent from the privilege revocation")
    if "#![forbid(unsafe_code)]" not in source[SOURCE_PATHS[7]]:
        errors.append("the pure engine must forbid unsafe code")
    compatibility = source[SOURCE_PATHS[5]]
    for check in ('c"server_version_num", b"180006"', 'c"block_size", b"8192"'):
        if check not in compatibility:
            errors.append(f"runtime server check is missing: {check}")
    if "compatibility::server();" not in source[SOURCE_PATHS[4]]:
        errors.append("preload must check runtime server presets")
    return errors


def check_manifest(root: Path) -> list[str]:
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    errors = []
    deps = manifest["workspace"]["dependencies"]
    for name in ("pgrx", "pgrx-pg-sys"):
        if deps[name]["version"] != "=0.19.2":
            errors.append(f"{name} version must match the audited toolchain")
    if manifest["profile"]["release"]["panic"] != "unwind":
        errors.append("host error guards require the unwind release profile")
    # a partial local checkout may omit unchanged blobs; CI must have the real lock.
    lock_path = root / "Cargo.lock"
    if not lock_path.exists():
        errors.append("Cargo.lock is absent; run this full check in a complete checkout")
    else:
        lock = tomllib.loads(lock_path.read_text())
        for name in ("pgrx", "pgrx-pg-sys"):
            versions = [p["version"] for p in lock["package"] if p["name"] == name]
            if versions != ["0.19.2"]:
                errors.append(f"locked {name} must resolve once to 0.19.2")
    return errors


def main() -> int:
    source = {path: (ROOT / path).read_text() for path in SOURCE_PATHS}
    errors = check_sources(source) + check_manifest(ROOT)
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print("G0 source contracts and locked pgrx versions match")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
