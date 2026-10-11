"""Tests for otlp_export.py: payload shape, skip behaviour and the HTTP POST.

Run: python3 -m unittest discover -s .github/actions/otlp-export -p 'test_*.py'
"""

from __future__ import annotations

import io
import json
import tempfile
import threading
import unittest
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import otlp_export

GITHUB_ENV = {
    "GITHUB_SERVER_URL": "https://github.com",
    "GITHUB_REPOSITORY": "jensholdgaard/ourios",
    "GITHUB_SHA": "0123456789abcdef0123456789abcdef01234567",
    "GITHUB_REF_NAME": "main",
    "GITHUB_REF_TYPE": "branch",
    "GITHUB_WORKFLOW": "Fuzz",
    "GITHUB_RUN_ID": "38045407298",
}

POINTS = [
    {"name": "ourios.fuzz.runs", "unit": "{run}", "type": "sum", "value": 1,
     "attributes": {"ourios.fuzz.target": "a", "ourios.fuzz.result": "ok"},
     "start_time": 100.0, "time": 400.0},
    {"name": "ourios.fuzz.runs", "unit": "{run}", "type": "sum", "value": 1,
     "attributes": {"ourios.fuzz.target": "b", "ourios.fuzz.result": "crash"},
     "start_time": 100.0, "time": 400.0},
    {"name": "ourios.fuzz.execution.rate", "unit": "{execution}/s", "type": "gauge",
     "value": 40316, "attributes": {"ourios.fuzz.target": "a"}, "time": 400.0},
    {"name": "ourios.fuzz.run.duration", "unit": "s", "type": "histogram", "value": 300.5,
     "attributes": {"ourios.fuzz.target": "a"}, "start_time": 100.0, "time": 400.5},
]


def attrs(kvs: list[dict]) -> dict:
    return {kv["key"]: next(iter(kv["value"].values())) for kv in kvs}


class BuildMetrics(unittest.TestCase):
    def test_points_group_by_name_with_otlp_json_encoding(self):
        metrics = {m["name"]: m for m in otlp_export.build_metrics(POINTS, now=0.0)}
        self.assertEqual(len(metrics), 3)

        runs = metrics["ourios.fuzz.runs"]
        self.assertEqual(runs["unit"], "{run}")
        self.assertEqual(runs["sum"]["aggregationTemporality"], 1)
        self.assertTrue(runs["sum"]["isMonotonic"])
        self.assertEqual(len(runs["sum"]["dataPoints"]), 2)
        first = runs["sum"]["dataPoints"][0]
        self.assertEqual(first["asInt"], "1")
        self.assertEqual(first["startTimeUnixNano"], "100000000000")
        self.assertEqual(first["timeUnixNano"], "400000000000")
        self.assertEqual(attrs(first["attributes"]), {"ourios.fuzz.result": "ok", "ourios.fuzz.target": "a"})

        gauge = metrics["ourios.fuzz.execution.rate"]["gauge"]["dataPoints"][0]
        self.assertEqual(gauge["asInt"], "40316")

        hist = metrics["ourios.fuzz.run.duration"]["histogram"]
        self.assertEqual(hist["aggregationTemporality"], 1)
        dp = hist["dataPoints"][0]
        self.assertEqual(
            {k: dp[k] for k in ("count", "sum", "min", "max", "bucketCounts", "explicitBounds")},
            {"count": "1", "sum": 300.5, "min": 300.5, "max": 300.5, "bucketCounts": ["1"], "explicitBounds": []},
        )

    def test_non_monotonic_sum_is_cumulative(self):
        (metric,) = otlp_export.build_metrics(
            [{"name": "system.cpu.logical.count", "unit": "{cpu}", "type": "sum", "monotonic": False, "value": 4}],
            now=7.0,
        )
        self.assertEqual(metric["sum"]["aggregationTemporality"], 2)
        self.assertFalse(metric["sum"]["isMonotonic"])
        self.assertEqual(metric["sum"]["dataPoints"][0]["timeUnixNano"], "7000000000")

    def test_double_values(self):
        (metric,) = otlp_export.build_metrics(
            [{"name": "x", "unit": "s", "type": "gauge", "value": 1.25}], now=0.0
        )
        self.assertEqual(metric["gauge"]["dataPoints"][0]["asDouble"], 1.25)

    def test_rejects_inconsistent_or_malformed_points(self):
        for points in (
            [{"name": "x", "unit": "s", "type": "gauge", "value": 1},
             {"name": "x", "unit": "By", "type": "gauge", "value": 1}],
            [{"name": "x", "unit": "s", "type": "summary", "value": 1}],
            [{"name": "x", "unit": "s", "type": "gauge"}],
            [{"name": "x", "unit": "s", "type": "gauge", "value": "1"}],
            [{"name": "x", "unit": "s", "type": "gauge", "value": True}],
        ):
            with self.subTest(points), self.assertRaises(otlp_export.InputError):
                otlp_export.build_metrics(points, now=0.0)


class Resource(unittest.TestCase):
    def test_ci_resource_uses_upstream_vcs_and_cicd_names(self):
        self.assertEqual(
            otlp_export.ci_resource(GITHUB_ENV),
            {
                "vcs.repository.url.full": "https://github.com/jensholdgaard/ourios",
                "vcs.ref.head.revision": GITHUB_ENV["GITHUB_SHA"],
                "vcs.ref.head.name": "main",
                "vcs.ref.head.type": "branch",
                "cicd.pipeline.name": "Fuzz",
                "cicd.pipeline.run.id": "38045407298",
                "cicd.pipeline.run.url.full": "https://github.com/jensholdgaard/ourios/actions/runs/38045407298",
            },
        )

    def test_ci_resource_outside_ci_is_empty(self):
        self.assertEqual(otlp_export.ci_resource({}), {})

    def test_resource_lines(self):
        self.assertEqual(
            otlp_export.parse_resource_lines("cicd.pipeline.task.name=smoke-fuzz a b\n\n  k = v=w \n"),
            {"cicd.pipeline.task.name": "smoke-fuzz a b", "k": "v=w"},
        )
        with self.assertRaises(otlp_export.InputError):
            otlp_export.parse_resource_lines("no-equals-sign")

    def test_proc_readers(self):
        with tempfile.TemporaryDirectory() as tmp:
            cpuinfo, meminfo = Path(tmp, "cpuinfo"), Path(tmp, "meminfo")
            cpuinfo.write_text("processor\t: 0\nmodel name\t: AMD EPYC 7763 64-Core Processor\n")
            meminfo.write_text("MemTotal:       16374356 kB\nMemFree:  1 kB\n")
            self.assertEqual(otlp_export.read_cpuinfo_model(cpuinfo), "AMD EPYC 7763 64-Core Processor")
            self.assertEqual(otlp_export.read_mem_total_bytes(meminfo), 16374356 * 1024)
            self.assertIsNone(otlp_export.read_cpuinfo_model(Path(tmp, "absent")))
            self.assertIsNone(otlp_export.read_mem_total_bytes(Path(tmp, "absent")))


class Capture(BaseHTTPRequestHandler):
    requests: list = []
    statuses: list = []

    def do_POST(self):  # noqa: N802 (http.server API)
        body = self.rfile.read(int(self.headers["Content-Length"]))
        Capture.requests.append((self.path, dict(self.headers), json.loads(body)))
        status, reply = Capture.statuses.pop(0) if Capture.statuses else (200, b"{}")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, *args):
        pass


class Main(unittest.TestCase):
    def setUp(self):
        Capture.requests, Capture.statuses = [], []
        self.server = HTTPServer(("127.0.0.1", 0), Capture)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.endpoint = f"http://127.0.0.1:{self.server.server_port}/"
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.metrics = Path(tmp.name, "metrics.json")
        self.metrics.write_text(json.dumps({"metrics": POINTS}))

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()

    def run_main(self, env: dict, *extra: str) -> tuple[int, str]:
        out = io.StringIO()
        argv = ["--metrics", str(self.metrics), "--service-name", "ourios-fuzz", *extra]
        with redirect_stdout(out):
            code = otlp_export.main(argv, env=env)
        return code, out.getvalue()

    def test_posts_otlp_json_with_bearer_token(self):
        env = {**GITHUB_ENV, "OTLP_EXPORT_ENDPOINT": self.endpoint, "OTLP_EXPORT_TOKEN": "s3cret"}
        code, out = self.run_main(env, "--resource-attributes", "cicd.pipeline.task.name=smoke-fuzz a", "--runner-spec")
        self.assertEqual(code, 0, out)
        self.assertNotIn("s3cret", out)
        ((path, headers, body),) = Capture.requests
        self.assertEqual(path, "/v1/metrics")
        self.assertEqual(headers["Authorization"], "Bearer s3cret")
        self.assertEqual(headers["Content-Type"], "application/json")
        (rm,) = body["resourceMetrics"]
        resource = attrs(rm["resource"]["attributes"])
        self.assertEqual(resource["service.name"], "ourios-fuzz")
        self.assertEqual(resource["cicd.pipeline.task.name"], "smoke-fuzz a")
        self.assertEqual(resource["vcs.ref.head.revision"], GITHUB_ENV["GITHUB_SHA"])
        self.assertIn("os.type", resource)
        names = {m["name"] for m in rm["scopeMetrics"][0]["metrics"]}
        self.assertIn("system.cpu.logical.count", names)
        self.assertIn("ourios.fuzz.runs", names)

    def test_skips_cleanly_without_endpoint_or_token(self):
        for env in (
            {"OTLP_EXPORT_ENDPOINT": self.endpoint, "OTLP_EXPORT_TOKEN": ""},
            {"OTLP_EXPORT_ENDPOINT": "", "OTLP_EXPORT_TOKEN": "s3cret"},
            {},
        ):
            with self.subTest(env):
                code, out = self.run_main(env)
                self.assertEqual(code, 0)
                self.assertIn("skipped", out)
        self.assertEqual(Capture.requests, [])

    def test_backend_rejection_is_a_warning_and_nonzero(self):
        Capture.statuses = [(401, b'{"message":"unauthorized"}')]
        env = {"OTLP_EXPORT_ENDPOINT": self.endpoint, "OTLP_EXPORT_TOKEN": "bad"}
        code, out = self.run_main(env)
        self.assertEqual(code, 1)
        self.assertIn("::warning::OTLP export failed: HTTP 401", out)
        self.assertEqual(len(Capture.requests), 1)

    def test_partial_success_rejection_is_reported(self):
        Capture.statuses = [(200, b'{"partialSuccess":{"rejectedDataPoints":"2","errorMessage":"bad unit"}}')]
        env = {"OTLP_EXPORT_ENDPOINT": self.endpoint, "OTLP_EXPORT_TOKEN": "t"}
        code, out = self.run_main(env)
        self.assertEqual(code, 1)
        self.assertIn("2 data points rejected: bad unit", out)

    def test_retries_a_transient_status(self):
        sleeps = []
        Capture.statuses = [(503, b""), (200, b"")]
        response = otlp_export.post(self.endpoint + "v1/metrics", "t", b"{}", sleep=sleeps.append)
        self.assertEqual(response, {})
        self.assertEqual(len(Capture.requests), 2)
        self.assertEqual(sleeps, [2])

    def test_dry_run_prints_the_payload_without_credentials(self):
        code, out = self.run_main({}, "--dry-run")
        self.assertEqual(code, 0)
        self.assertEqual(len(json.loads(out)["resourceMetrics"]), 1)
        self.assertEqual(Capture.requests, [])


if __name__ == "__main__":
    unittest.main()
