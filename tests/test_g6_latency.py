"""Tests for the PostgreSQL 18 pgbench log summarizer."""

import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("g6_latency", ROOT / "tools/g6_latency.py")
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class LatencySummaryTests(unittest.TestCase):
    def test_service_percentiles_and_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pgbench.log"
            path.write_text(
                "0 0 1000 0 1 0\n"
                "0 1 2000 0 1 0\n"
                "0 2 3000 0 1 0\n"
                "0 3 skipped 0 1 0\n"
                "0 3 failed 0 1 0\n",
                encoding="utf-8",
            )
            result = MODULE.summarize([path])
            self.assertEqual(result["service_latency"]["count"], 3)
            self.assertEqual(result["service_latency"]["p50_us"], 2000)
            self.assertEqual(result["service_latency"]["p95_us"], 3000)
            self.assertEqual(result["service_latency"]["p99_us"], 3000)
            self.assertEqual(result["failures"]["skipped"], 1)
            self.assertEqual(result["failures"]["failed"], 1)

    def test_rate_controlled_latency_includes_schedule_lag(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pgbench.log"
            path.write_text(
                "0 0 1000 0 1 0 500\n"
                "0 1 2000 0 1 0 100\n",
                encoding="utf-8",
            )
            result = MODULE.summarize([path], scheduled=True)
            self.assertEqual(result["scheduled_latency"]["p50_us"], 1500)
            self.assertEqual(result["scheduled_latency"]["p99_us"], 2100)

    def test_truncated_line_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pgbench.log"
            path.write_text("0 0 1000\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                MODULE.summarize([path])


if __name__ == "__main__":
    unittest.main()
