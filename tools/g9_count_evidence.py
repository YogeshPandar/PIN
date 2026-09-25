#!/usr/bin/env python3
"""Recompute historical controls from committed raw files, not new benchmarks."""
from __future__ import annotations

import argparse
from collections import defaultdict
import hashlib
import json
from pathlib import Path
import re
import statistics
import tarfile


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def archived_json(path: Path, suffix: str) -> tuple[object, dict]:
    with tarfile.open(path, 'r:gz') as archive:
        matches = [m for m in archive.getmembers() if m.isfile() and m.name.endswith(suffix)]
        if len(matches) != 1:
            raise ValueError(f'{path}: expected exactly one {suffix}')
        source = archive.extractfile(matches[0])
        if source is None:
            raise ValueError('missing member')
        data = source.read()
        return json.loads(data), {'archive': str(path), 'archive_sha256': digest(path.read_bytes()),
                                  'member': matches[0].name, 'member_sha256': digest(data)}


def reanalyze(root: Path) -> dict:
    pr21 = root / 'docs/runs/2026-09-25-pr21-paired-cpu'
    raw, source = archived_json(pr21/'same-backend-toggle.tar.gz', '/results.json')
    groups = defaultdict(list)
    for sample in raw['results']:
        groups[(sample['case'], sample['mode'])].append(sample)
    controls = []
    for case in sorted({key[0] for key in groups}):
        modes = {}
        for mode in ('off', 'on', 'gin'):
            samples = groups[(case, mode)]
            modes[mode] = {'median_cpu_us': statistics.median(s['cpu_us_per_query'] for s in samples),
                          'median_wall_us': statistics.median(s['wall_us_per_query'] for s in samples),
                          'matched_rows': sorted({s['rows'] for s in samples}),
                          'samples': len(samples), 'queries': [s['queries'] for s in samples]}
        if any(modes[m]['matched_rows'] != modes['gin']['matched_rows'] for m in modes):
            raise ValueError('historical control cardinality mismatch')
        controls.append({'historical_label': case, 'modes': modes,
            'owner_on_speedup_vs_off': modes['off']['median_cpu_us']/modes['on']['median_cpu_us'],
            'owner_on_speedup_vs_gin': modes['gin']['median_cpu_us']/modes['on']['median_cpu_us'],
            'selectivity': [n/raw['total_rows'] for n in modes['on']['matched_rows']]})
    maintenance = []
    for mode in ('on', 'off'):
        rows, provenance = archived_json(pr21/f'owner-{mode}-evidence.tar.gz', '/write-maintenance.json')
        by_engine = {r['engine']: r for r in rows}
        metrics = {e: {'backend_cpu_ns': row['lifecycle_backend_cpu_ns'],
                       'wal_bytes': row['lifecycle_cluster_wal_bytes'],
                       'index_bytes': row['heap_and_all_indexes_bytes'][1]}
                   for e, row in by_engine.items()}
        maintenance.append({'owner_gate': mode, 'source': provenance, 'metrics': metrics,
            'pin_over_gin': {k: metrics['pin'][k]/metrics['gin'][k] for k in metrics['pin']}})
    profiles = []
    directory = root / 'docs/runs/2026-09-24-g9-cpu-profile/raw'
    paths = [directory/'g9-cpu-full'/f'{case}-grouped-perf.dso.txt'
             for case in ('rare', 'selective_and', 'and')]
    paths += [directory/'g9-cpu-broad-profile'/f'{case}-grouped-perf.dso.txt'
              for case in ('common', 'or')]
    paths += sorted(directory.glob('g9-cpu-write*/*_pin_*-perf.dso.txt'))
    for path in paths:
        text = path.read_text()
        dso = {name: float(share) for share, name in re.findall(r'^\s*(\d+\.\d+)%\s+(\S+)\s*$',text,re.M)}
        flat = path.with_name(path.name.replace('.dso.', '.flat.'))
        flat_text = flat.read_text()
        symbols = [{'self_percent': float(p), 'dso': d, 'symbol': symbol.strip()}
                   for p, d, symbol in re.findall(r'^\s*(\d+\.\d+)%\s+(\S+)\s+\[.\]\s+(.*?)\s+-\s+-\s*$',flat_text,re.M)]
        profiles.append({'file': str(path), 'sha256': digest(path.read_bytes()), 'dso_self_percent': dso,
                         'flat_file': str(flat), 'flat_sha256': digest(flat.read_bytes()),
                         'top_15_self_symbols': symbols[:15]})
    result = {'kind': 'historical-reanalysis-not-new-performance', 'revision': raw['revision'],
              'source': source, 'fixture_rows': raw['total_rows'], 'related_new_rows': raw['added_rows'],
              'controls': controls, 'maintenance': maintenance, 'sampled_profiles': profiles,
              'limits': ['small warm VM fixtures', 'older G9 profiles are not PR21 phase CPU',
                         'DSO self samples are not additive call-chain timings',
                         'historical rare label is not rare after related writes',
                         'no grouped COUNT native measurements', 'no matched TIN run']}
    # make results independent of the checkout's absolute path.
    return json.loads(json.dumps(result).replace(str(root.resolve())+'/', ''))


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    args.output.write_text(json.dumps(reanalyze(args.root.resolve()), indent=2, allow_nan=False)+'\n')


if __name__ == '__main__':
    main()
