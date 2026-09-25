#!/usr/bin/env python3
"""Attribute archived plan work without claiming a new benchmark or a CPU profile."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path, PurePosixPath
import tarfile

COUNTERS = (
    'Candidate Owners', 'VM Probes', 'VM Certified Roots', 'Heap Fetches',
    'Heap Matches', 'Grouped Result Pages', 'Grouped Result Roots',
    'Scalar Result Roots', 'Grouped Count Runs', 'Index Page Reads',
    'Index Payload Bytes', 'Decoded Term Page Payloads', 'Decoded Term Offset Bytes',
    'Candidate Heap Pages', 'Live Heap Pages', 'Delta Segment Visits',
    'Mutable Frontier Span',
)
MAX_JSON_BYTES = 8 << 20


def ratio(numerator: float, denominator: float) -> float | None:
    if not all(math.isfinite(v) and v >= 0 for v in (numerator, denominator)):
        raise ValueError('invalid nonnegative measurement')
    return numerator / denominator if denominator else None


def plan_work(plan: list) -> list[dict]:
    if not isinstance(plan, list) or len(plan) != 1 or 'Plan' not in plan[0]:
        raise ValueError('expected one PostgreSQL JSON EXPLAIN')
    output = []
    pending = [plan[0]['Plan']]
    while pending:
        node = pending.pop()
        pending.extend(node.get('Plans', []))
        if node.get('Custom Plan Provider') != 'PinCount' or node.get('Actual Loops', 0) == 0:
            continue
        counters = {name: node[name] for name in COUNTERS if name in node}
        if any(type(value) not in (int, float) or not math.isfinite(value) or value < 0
               for value in counters.values()):
            raise ValueError('invalid plan counter')
        output.append({
            'counters': counters,
            'payload_to_decoded_offset_bytes': ratio(counters.get('Index Payload Bytes', 0),
                                                     counters.get('Decoded Term Offset Bytes', 0)),
            'vm_probes_per_candidate': ratio(counters.get('VM Probes', 0),
                                             counters.get('Candidate Owners', 0)),
            'heap_fetch_fraction': ratio(counters.get('Heap Fetches', 0),
                                         counters.get('Candidate Owners', 0)),
        })
    return output


def reanalyze(path: Path) -> dict:
    plans = []
    summary = None
    names = set()
    total = 0
    with tarfile.open(path, 'r:gz') as archive:
        for member in archive:
            if not member.isfile() or not member.name.endswith('.json'):
                continue
            if member.name in names or len(names) >= 4096:
                raise ValueError('duplicate or excessive archive members')
            names.add(member.name)
            total += member.size
            if member.size > MAX_JSON_BYTES or total > 128 << 20:
                raise ValueError('archive JSON budget exceeded')
            if not (member.name.endswith('-plan.json') or member.name.endswith('/summary.json')):
                continue
            stream = archive.extractfile(member)
            if stream is None:
                raise ValueError('missing archive member')
            raw = stream.read(MAX_JSON_BYTES + 1)
            if len(raw) != member.size:
                raise ValueError('invalid archive member size')
            value = json.loads(raw)
            if member.name.endswith('/summary.json'):
                if summary is not None:
                    raise ValueError('multiple summaries')
                summary = value
            else:
                plans.append({'member': member.name, 'sha256': hashlib.sha256(raw).hexdigest(),
                              'stage': PurePosixPath(member.name).parent.name,
                              'executed_pin_count': plan_work(value)})
    comparisons = []
    if summary is not None:
        by_key = {}
        for row in summary:
            key = (row['rows'], row['stage'], row['case'], row['mode'])
            if key in by_key:
                raise ValueError('duplicate summary measurement')
            by_key[key] = row
        for key, row in sorted(by_key.items()):
            if row['mode'] == 'gin':
                continue
            control = by_key.get((*key[:3], 'gin'))
            if control is None:
                raise ValueError('missing matched GIN control')
            value = ratio(row['median_batch_cpu_us'], control['median_batch_cpu_us'])
            comparisons.append({**dict(zip(('rows', 'stage', 'case', 'mode'), key)),
                                'pin_over_gin_cpu': value,
                                'at_most_ten_percent': value is not None and value <= 0.1,
                                'tail_sample_warning': row.get('tail_sample_warning', True)})
    with path.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
    return {'kind': 'historical-plan-reanalysis-not-new-performance',
            'archive': path.name, 'archive_sha256': digest,
            'limitations': ['plan counters are work counts, not sampled CPU attribution',
                            'no timing is inferred from bytes or heap fetch counts',
                            'pin_previous in PR23 is a same-binary consumer control, not an older binary'],
            'comparisons': comparisons, 'plans': sorted(plans, key=lambda plan: plan['member'])}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(reanalyze(args.archive), indent=2, allow_nan=False) + '\n')


if __name__ == '__main__':
    main()
