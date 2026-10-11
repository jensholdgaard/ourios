"""Tests for fuzz_stats.py against captured libFuzzer logs.

`testdata/fuzz/ok.log` and `oom.log` are trimmed from real `fuzz.yml` runs
(the 2026-10-10 otlp_json run and the 2026-09-02 dsl_parse OOM). Those runs
predate `-print_final_stats=1`, so their `stat::` blocks are appended in
libFuzzer's format. `crash.log` (a Rust panic) and `timeout.log` are
synthetic in the same format.

Run: python3 -m unittest discover -s .github/scripts -p 'test_*.py'
"""

from __future__ import annotations

import io
import json
import unittest
from contextlib import redirect_stdout
from pathlib import Path

import fuzz_stats

FIXTURES = Path(__file__).parent / "testdata" / "fuzz"


def fixture(name: str) -> str:
    return (FIXTURES / name).read_text()


def by_name(points: list[dict]) -> dict[str, dict]:
    names = [p["name"] for p in points]
    assert len(names) == len(set(names)), names
    return {p["name"]: p for p in points}


class ParseLog(unittest.TestCase):
    def test_normal_run_takes_the_done_line_and_final_stats(self):
        self.assertEqual(
            fuzz_stats.parse_log(fixture("ok.log")),
            {
                "coverage": 3742,
                "features": 14525,
                "corpus_inputs": 4205,
                "corpus_bytes": 676 * 1024,
                "executions": 12135052,
                "exec_per_sec": 40316,
                "peak_rss_bytes": 692 * 1024 * 1024,
            },
        )

    def test_oom_run_takes_the_last_status_line_before_the_report(self):
        parsed = fuzz_stats.parse_log(fixture("oom.log"))
        self.assertEqual(parsed["coverage"], 3492)
        self.assertEqual(parsed["features"], 12398)
        self.assertEqual(parsed["corpus_inputs"], 2646)
        self.assertEqual(parsed["corpus_bytes"], 304 * 1024)
        self.assertEqual(parsed["peak_rss_bytes"], 2051 * 1024 * 1024)

    def test_crash_run_parses_byte_and_kilobyte_corpus_sizes(self):
        parsed = fuzz_stats.parse_log(fixture("crash.log"))
        self.assertEqual(parsed["corpus_inputs"], 89)
        self.assertEqual(parsed["corpus_bytes"], 18 * 1024)
        self.assertEqual(parsed["executions"], 300418)

    def test_megabyte_corpus(self):
        parsed = fuzz_stats.parse_log("#9\tpulse  cov: 1 ft: 2 corp: 3/17Mb exec/s: 0 rss: 1Mb\n")
        self.assertEqual(parsed["corpus_bytes"], 17 << 20)

    def test_empty_log_yields_nothing(self):
        self.assertEqual(fuzz_stats.parse_log(""), {})


class Classify(unittest.TestCase):
    def test_successful_step_is_ok(self):
        self.assertEqual(fuzz_stats.classify(fixture("ok.log"), "success"), "ok")

    def test_findings_by_log_marker(self):
        for name, expected in (
            ("crash.log", "crash"),
            ("oom.log", "oom"),
            ("timeout.log", "timeout"),
        ):
            with self.subTest(name):
                self.assertEqual(fuzz_stats.classify(fixture(name), "failure"), expected)

    def test_leak_is_not_reported_as_a_generic_crash(self):
        log = "==77==ERROR: LeakSanitizer: detected memory leaks\nSUMMARY: AddressSanitizer: 64 byte(s) leaked\n"
        self.assertEqual(fuzz_stats.classify(log, "failure"), "leak")

    def test_failure_without_a_finding_is_error(self):
        self.assertEqual(fuzz_stats.classify("error: could not compile `ourios-fuzz`\n", "failure"), "error")
        self.assertEqual(fuzz_stats.classify("", "skipped"), "error")

    def test_cancelled_wins_over_a_partial_log(self):
        self.assertEqual(fuzz_stats.classify(fixture("crash.log"), "cancelled"), "cancelled")


class BuildPoints(unittest.TestCase):
    def test_normal_run(self):
        points = by_name(
            fuzz_stats.build_points(
                target="otlp_json",
                result="ok",
                parsed=fuzz_stats.parse_log(fixture("ok.log")),
                budget=300,
                start=1000.0,
                end=1301.5,
                now=2000.0,
            )
        )
        self.assertEqual(
            sorted(points),
            [
                "ourios.fuzz.corpus.input.count",
                "ourios.fuzz.corpus.size",
                "ourios.fuzz.coverage",
                "ourios.fuzz.execution.rate",
                "ourios.fuzz.executions",
                "ourios.fuzz.feature.count",
                "ourios.fuzz.memory.peak",
                "ourios.fuzz.run.budget",
                "ourios.fuzz.run.duration",
                "ourios.fuzz.runs",
            ],
        )
        runs = points["ourios.fuzz.runs"]
        self.assertEqual((runs["type"], runs["unit"], runs["value"]), ("sum", "{run}", 1))
        self.assertEqual(
            runs["attributes"], {"ourios.fuzz.target": "otlp_json", "ourios.fuzz.result": "ok"}
        )
        self.assertEqual((runs["start_time"], runs["time"]), (1000.0, 1301.5))
        duration = points["ourios.fuzz.run.duration"]
        self.assertEqual((duration["type"], duration["unit"], duration["value"]), ("histogram", "s", 301.5))
        self.assertEqual(points["ourios.fuzz.run.budget"]["attributes"], {"ourios.fuzz.target": "otlp_json"})
        self.assertEqual(points["ourios.fuzz.coverage"]["value"], 3742)
        self.assertEqual(points["ourios.fuzz.execution.rate"]["unit"], "{execution}/s")
        self.assertEqual(points["ourios.fuzz.executions"]["type"], "sum")

    def test_run_that_never_started_still_reports_its_result(self):
        points = by_name(
            fuzz_stats.build_points(
                target="wal_frame", result="error", parsed={}, budget=60, start=None, end=None, now=5.0
            )
        )
        self.assertEqual(sorted(points), ["ourios.fuzz.run.budget", "ourios.fuzz.runs"])
        self.assertEqual(points["ourios.fuzz.runs"]["attributes"]["ourios.fuzz.result"], "error")
        self.assertEqual(points["ourios.fuzz.runs"]["time"], 5.0)
        self.assertNotIn("start_time", points["ourios.fuzz.runs"])


class Main(unittest.TestCase):
    def run_main(self, *argv: str) -> dict:
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(fuzz_stats.main(list(argv)), 0)
        return json.loads(out.getvalue())

    def test_crash_run_end_to_end(self):
        doc = self.run_main(
            "--log", str(FIXTURES / "crash.log"), "--target", "wal_frame",
            "--step-outcome", "failure", "--budget", "300", "--start", "10", "--end", "20",
        )
        points = by_name(doc["metrics"])
        self.assertEqual(points["ourios.fuzz.runs"]["attributes"]["ourios.fuzz.result"], "crash")
        self.assertEqual(points["ourios.fuzz.run.duration"]["value"], 10.0)
        self.assertEqual(points["ourios.fuzz.memory.peak"]["value"], 143 << 20)

    def test_missing_log_and_blank_times(self):
        doc = self.run_main(
            "--log", str(FIXTURES / "absent.log"), "--target", "dsl_parse",
            "--step-outcome", "skipped", "--budget", "60", "--start", "", "--end", "",
        )
        points = by_name(doc["metrics"])
        self.assertEqual(points["ourios.fuzz.runs"]["attributes"]["ourios.fuzz.result"], "error")
        self.assertNotIn("ourios.fuzz.run.duration", points)


if __name__ == "__main__":
    unittest.main()
