#!/usr/bin/env python3
"""Losslessly archive raw G9 CPU profiles and text evidence for Git."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import subprocess


def archive_profile(source: Path, destination: Path) -> dict:
    target = destination / (source.name + '.gz')
    original_hash = hashlib.sha256()
    original_size = 0
    reader = subprocess.Popen(['sudo', '-n', 'cat', str(source)],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        with target.open('wb') as output, gzip.GzipFile(
            filename='', mode='wb', fileobj=output, mtime=0, compresslevel=9,
        ) as compressed:
            assert reader.stdout is not None
            while chunk := reader.stdout.read(1024 * 1024):
                original_hash.update(chunk)
                original_size += len(chunk)
                compressed.write(chunk)
        error = reader.communicate()[1]
        if reader.returncode != 0:
            raise RuntimeError(f'could not read {source}: {error.decode(errors="replace")}')
    finally:
        if reader.poll() is None:
            reader.kill()
            reader.wait()
    return {
        'file': target.name,
        'raw_file': source.name,
        'raw_bytes': original_size,
        'raw_sha256': original_hash.hexdigest(),
        'gzip_bytes': target.stat().st_size,
        'gzip_sha256': hashlib.sha256(target.read_bytes()).hexdigest(),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--destination', type=Path, required=True)
    parser.add_argument('--update', action='store_true')
    args = parser.parse_args()
    args.destination.mkdir(parents=True, exist_ok=args.update)
    manifest = []
    for source in sorted(args.source.iterdir()):
        if not source.is_file():
            continue
        if source.name.endswith('.data'):
            manifest.append(archive_profile(source, args.destination))
        else:
            target = args.destination / source.name
            shutil.copyfile(source, target)
            manifest.append({
                'file': target.name,
                'bytes': target.stat().st_size,
                'sha256': hashlib.sha256(target.read_bytes()).hexdigest(),
            })
    (args.destination / 'MANIFEST.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    main()
