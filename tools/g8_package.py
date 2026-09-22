#!/usr/bin/env python3
"""Assemble and verify a qualification bundle, never install or publish it.

Inputs are a private normal-build staging directory. Archives have fixed names,
metadata and order. Verification never extracts files. Contracts: g8-package in
docs/api-evidence.md. Checksums establish integrity, not publisher authenticity.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import re
import sys
import tarfile
from pathlib import Path
from typing import BinaryIO

CHUNK = 1024 * 1024
MAX_FILE = 512 * CHUNK
MAX_TOTAL = 768 * CHUNK
MAX_MANIFEST = CHUNK
PAYLOAD = (
    "extension/pin--0.0.0.sql",
    "extension/pin.control",
    "lib/pin.so",
    "metadata/Cargo.lock",
    "metadata/build.json",
    "metadata/dependencies.json",
)
HOOK = re.compile(rb"\b(?:g0_[A-Za-z0-9_]*|g2_inject[A-Za-z0-9_]*|pin_test_[A-Za-z0-9_]*)\b")


class PackageError(ValueError):
    """The bundle is incomplete, incompatible or not a normal build."""


def encoded(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, indent=2, allow_nan=False) + "\n").encode()


def digest(stream: BinaryIO, size: int) -> str:
    """Hash exactly size bytes with fixed scratch; reject short streams."""
    if not 0 <= size <= MAX_FILE:
        raise PackageError("member exceeds the package size limit")
    result = hashlib.sha256()
    remaining = size
    while remaining:
        chunk = stream.read(min(CHUNK, remaining))
        if not chunk:
            raise PackageError("truncated member")
        result.update(chunk)
        remaining -= len(chunk)
    return result.hexdigest()


def validate_metadata(build: dict, dependencies: dict) -> None:
    required = {
        "schema": 1, "status": "qualification-only", "profile": "normal",
        "postgres": "18.6", "rust": "1.98.1", "pgrx": "0.19.2",
        "target": "x86_64-unknown-linux-gnu", "sql_version": "0.0.0",
    }
    if any(build.get(key) != value for key, value in required.items()):
        raise PackageError("incompatible build metadata")
    if not re.fullmatch(r"[0-9a-f]{40}", build.get("commit", "")):
        raise PackageError("missing source commit")
    nodes = dependencies.get("resolve", {}).get("nodes", [])
    packages = {package["id"]: package for package in dependencies.get("packages", [])}
    host = [node for node in nodes if packages.get(node["id"], {}).get("name") == "pin-pg"]
    if len(host) != 1 or set(host[0]["features"]) - {"default", "pg18"} or "pg18" not in host[0]["features"]:
        raise PackageError("normal pg18 feature graph is required")
    for name in ("pgrx", "pgrx-pg-sys"):
        versions = {p["version"] for p in packages.values() if p["name"] == name}
        if versions != {"0.19.2"}:
            raise PackageError(f"incompatible {name} dependency")


def inspect_stage(stage: Path) -> dict[str, dict[str, int | str]]:
    """Validate regular staged files and hash the exact deployment payload."""
    actual = set()
    for path in stage.rglob("*"):
        if path.is_symlink():
            raise PackageError("staging links are not allowed")
        if path.is_file():
            actual.add(path.relative_to(stage).as_posix())
        elif not path.is_dir():
            raise PackageError("staging special files are not allowed")
    if actual != set(PAYLOAD):
        raise PackageError("unexpected or missing staged file")
    result = {}
    total = 0
    for name in PAYLOAD:
        path = stage / name
        size = path.stat().st_size
        total += size
        if size == 0 or total > MAX_TOTAL:
            raise PackageError("empty member or oversized package")
        with path.open("rb") as stream:
            result[name] = {"size": size, "sha256": digest(stream, size)}
    for name in ("metadata/build.json", "metadata/dependencies.json", "extension/pin--0.0.0.sql", "extension/pin.control"):
        if result[name]["size"] > 8 * CHUNK:
            raise PackageError("oversized metadata or SQL")
    build = json.loads((stage / "metadata/build.json").read_bytes())
    dependencies = json.loads((stage / "metadata/dependencies.json").read_bytes())
    validate_metadata(build, dependencies)
    sql = (stage / "extension/pin--0.0.0.sql").read_bytes()
    if HOOK.search(sql) or b"build_profile" not in sql or b"build_revision" not in sql:
        raise PackageError("SQL is missing build identity or contains test hooks")
    control = (stage / "extension/pin.control").read_text(encoding="ascii")
    for setting in ("default_version = '0.0.0'", "superuser = true", "trusted = false", "relocatable = false", "schema = 'pin'"):
        if setting not in control.splitlines():
            raise PackageError("unexpected extension control policy")
    with (stage / "lib/pin.so").open("rb") as library:
        header = library.read(20)
    if len(header) != 20 or header[:7] != b"\x7fELF\x02\x01\x01" or header[16:20] != b"\x03\x00\x3e\x00":
        raise PackageError("expected an x86_64 little-endian ELF shared library")
    return result


def assemble(stage: Path, output: Path) -> None:
    """Write a new deterministic archive; leave existing outputs untouched."""
    members = inspect_stage(stage)
    manifest = encoded({"schema": 1, "files": members})
    # the private staging directory must not be modified while assembling.
    with output.open("xb") as destination:
        try:
            with gzip.GzipFile(filename="", mode="wb", fileobj=destination, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.USTAR_FORMAT) as archive:
                    info = tarfile.TarInfo("manifest.json")
                    info.size = len(manifest)
                    info.mode = 0o644
                    archive.addfile(info, io.BytesIO(manifest))
                    for name in PAYLOAD:
                        info = tarfile.TarInfo(name)
                        info.size = members[name]["size"]
                        info.mode = 0o644
                        with (stage / name).open("rb") as source:
                            archive.addfile(info, source)
            destination.flush()
            verify(output)
        except BaseException:
            output.unlink(missing_ok=True)
            raise


def verify(path: Path) -> None:
    """Validate the strict archive inventory without writing archive paths."""
    with tarfile.open(path, "r|gz") as archive:
        first = archive.next()
        if first is None or first.name != "manifest.json" or not first.isfile() or not 0 < first.size <= MAX_MANIFEST:
            raise PackageError("missing or oversized manifest")
        stream = archive.extractfile(first)
        if stream is None:
            raise PackageError("unreadable manifest")
        manifest = json.loads(stream.read(MAX_MANIFEST + 1))
        if manifest.get("schema") != 1 or set(manifest.get("files", {})) != set(PAYLOAD):
            raise PackageError("unexpected manifest inventory")
        seen = set()
        total = 0
        for member in archive:
            # streaming iteration can yield the already-consumed first member.
            if member is first:
                continue
            if member.name not in PAYLOAD or member.name in seen or not member.isfile() or member.pax_headers:
                raise PackageError("unexpected, duplicate or nonregular member")
            if member.mode != 0o644 or member.uid != 0 or member.gid != 0 or member.mtime != 0:
                raise PackageError("unexpected archive metadata")
            seen.add(member.name)
            total += member.size
            expected = manifest["files"][member.name]
            if member.size <= 0 or total > MAX_TOTAL or expected.get("size") != member.size:
                raise PackageError("invalid member size")
            stream = archive.extractfile(member)
            if stream is None or digest(stream, member.size) != expected.get("sha256"):
                raise PackageError("member checksum mismatch")
        if seen != set(PAYLOAD):
            raise PackageError("missing payload member")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("assemble")
    build.add_argument("stage", type=Path)
    build.add_argument("output", type=Path)
    check = commands.add_parser("verify")
    check.add_argument("archive", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "assemble":
            assemble(args.stage, args.output)
        else:
            verify(args.archive)
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError, EOFError) as error:
        print(f"G8 package rejected: {error}", file=sys.stderr)
        return 1
    print("G8 package integrity verified; release qualification is separate")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
