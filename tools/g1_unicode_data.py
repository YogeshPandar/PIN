"""Fetch immutable Unicode 16 conformance inputs and verify Git object hashes."""

import hashlib
import json
import pathlib
import sys
import urllib.request

COMMIT = "1882e4cca24a298184d685e6a3820428749d050d"
ROOT = f"https://raw.githubusercontent.com/unicode-org/unicodetools/{COMMIT}/"
UCD = "unicodetools/data/ucd/16.0.0/"
FILES = {
    "NormalizationTest.txt": (UCD + "NormalizationTest.txt", "3aae8f72e873dfc3c1e876775859e6090b3c39f9"),
    "WordBreakTest.txt": (UCD + "auxiliary/WordBreakTest.txt", "2fededd0b8c260d2011cacefbc4093194c8c14a9"),
    "CaseFolding.txt": (UCD + "CaseFolding.txt", "1b7a9c156c7cf4256bbe685dc45f78ac68d51e2d"),
    "LICENSE": ("LICENSE", "d7e7973c2fd6f2586a8999a69dc21e39af26be0f"),
}
MAX_BYTES = 8 << 20


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: python tools/g1_unicode_data.py DESTINATION")
    directory = pathlib.Path(sys.argv[1])
    directory.mkdir(parents=True, exist_ok=True)
    manifest = {"commit": COMMIT, "unicode": "16.0.0", "files": {}}
    for name, (path, expected) in FILES.items():
        destination = directory / name
        if destination.exists():
            data = destination.read_bytes()
        else:
            request = urllib.request.Request(ROOT + path, headers={"User-Agent": "PIN-conformance"})
            with urllib.request.urlopen(request, timeout=60) as response:
                data = response.read(MAX_BYTES + 1)
        if len(data) > MAX_BYTES:
            raise ValueError(f"oversized conformance file: {name}")
        object_id = hashlib.sha1(f"blob {len(data)}\0".encode() + data).hexdigest()
        if object_id != expected:
            raise ValueError(f"conformance hash mismatch: {name}: {object_id}")
        temporary = directory / (name + ".tmp")
        temporary.write_bytes(data)
        temporary.replace(destination)
        manifest["files"][name] = {"git_blob": object_id, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
