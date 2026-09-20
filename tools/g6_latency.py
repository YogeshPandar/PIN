#!/usr/bin/env python3
"""Summarize PostgreSQL 18 pgbench per-transaction logs."""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
from statistics import fmean
from typing import Iterable


_FAILURES = {"skipped", "failed", "serialization", "deadlock"}


def _percentile(values: list[int], fraction: float) -> int | None:
    if not values:
        return None
    rank = max(0, math.ceil(fraction * len(values)) - 1)
    return values[rank]


def summarize(paths: Iterable[Path], scheduled: bool = False) -> dict[str, object]:
    service: list[int] = []
    end_to_end: list[int] = []
    failures = {name: 0 for name in sorted(_FAILURES)}

    files = 0
    lines = 0
    for path in paths:
        files += 1
        with path.open("r", encoding="utf-8") as source:
            for number, raw in enumerate(source, 1):
                raw = raw.strip()
                if not raw:
                    continue
                lines += 1
                fields = raw.split()
                if len(fields) < 6:
                    raise ValueError(f"{path}:{number}: truncated pgbench log line")
                duration = fields[2]
                if duration in _FAILURES:
                    failures[duration] += 1
                    continue
                try:
                    elapsed = int(duration)
                except ValueError as error:
                    raise ValueError(
                        f"{path}:{number}: invalid pgbench duration {duration!r}"
                    ) from error
                if elapsed < 0:
                    raise ValueError(f"{path}:{number}: negative transaction duration")
                service.append(elapsed)
                if scheduled:
                    if len(fields) < 7:
                        raise ValueError(
                            f"{path}:{number}: rate-controlled log lacks schedule lag"
                        )
                    lag = int(fields[6])
                    end_to_end.append(elapsed + lag)

    if files == 0:
        raise ValueError("no pgbench log files")
    service.sort()
    end_to_end.sort()

    def stats(values: list[int]) -> dict[str, float | int | None]:
        return {
            "count": len(values),
            "mean_us": fmean(values) if values else None,
            "p50_us": _percentile(values, 0.50),
            "p95_us": _percentile(values, 0.95),
            "p99_us": _percentile(values, 0.99),
            "max_us": values[-1] if values else None,
        }

    result: dict[str, object] = {
        "files": files,
        "lines": lines,
        "service_latency": stats(service),
        "failures": failures,
    }
    if scheduled:
        result["scheduled_latency"] = stats(end_to_end)
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scheduled", action="store_true")
    parser.add_argument("logs", nargs="+", type=Path)
    args = parser.parse_args()
    print(json.dumps(summarize(args.logs, args.scheduled), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
