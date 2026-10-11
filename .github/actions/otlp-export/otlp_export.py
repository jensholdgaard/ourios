#!/usr/bin/env python3
"""POST a JSON file of metric points to an OTLP/HTTP endpoint as OTLP/JSON.

Input (`--metrics`):

    {"metrics": [{"name": "...", "unit": "...", "type": "gauge|sum|histogram",
                  "value": 1.5, "attributes": {"k": "v"},
                  "time": <epoch s>, "start_time": <epoch s>,
                  "monotonic": true, "description": "..."}]}

`type` is the OTLP data type: a `sum` is a monotonic delta over
[start_time, time] unless `"monotonic": false`, which makes it a cumulative
non-monotonic sum (an UpDownCounter reading); a `histogram` is one
observation. Points sharing a name become one metric.

The endpoint and token come only from the environment (OTLP_EXPORT_ENDPOINT,
the OTLP/HTTP base URL; OTLP_EXPORT_TOKEN, sent as a bearer token), so the
token never appears in argv. Either one empty means "not configured": the
script prints a notice and exits 0. Standard library only.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

SCOPE_NAME = "ourios.ci.otlp-export"
RETRYABLE_STATUS = {429, 502, 503, 504}
ATTEMPTS = 3
TIMEOUT_S = 15
AGGREGATION_TEMPORALITY_DELTA = 1
AGGREGATION_TEMPORALITY_CUMULATIVE = 2

# platform.machine() spellings mapped to the `host.arch` well-known values.
HOST_ARCH = {
    "x86_64": "amd64",
    "amd64": "amd64",
    "aarch64": "arm64",
    "arm64": "arm64",
    "armv7l": "arm32",
    "i386": "x86",
    "i686": "x86",
    "ppc64le": "ppc64",
    "s390x": "s390x",
}


class InputError(Exception):
    pass


def any_value(value) -> dict:
    if isinstance(value, bool):
        return {"boolValue": value}
    if isinstance(value, int):
        return {"intValue": str(value)}
    if isinstance(value, float):
        return {"doubleValue": value}
    if isinstance(value, str):
        return {"stringValue": value}
    if isinstance(value, (list, tuple)):
        return {"arrayValue": {"values": [any_value(v) for v in value]}}
    raise InputError(f"unsupported attribute value {value!r}")


def key_values(attributes: dict) -> list[dict]:
    return [{"key": k, "value": any_value(v)} for k, v in sorted(attributes.items())]


def nanos(epoch_seconds: float) -> str:
    return str(int(round(epoch_seconds * 1e9)))


def number_value(value) -> dict:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise InputError(f"metric value must be a number, got {value!r}")
    return {"asInt": str(value)} if isinstance(value, int) else {"asDouble": float(value)}


def data_point(point: dict, now: float) -> dict:
    end = float(point.get("time", now))
    start = float(point.get("start_time", end))
    dp = {
        "attributes": key_values(point.get("attributes", {})),
        "startTimeUnixNano": nanos(start),
        "timeUnixNano": nanos(end),
    }
    if point["type"] == "histogram":
        value = point["value"]
        number_value(value)
        dp.update(
            count="1",
            sum=float(value),
            min=float(value),
            max=float(value),
            bucketCounts=["1"],
            explicitBounds=[],
        )
    else:
        dp.update(number_value(point["value"]))
    return dp


def build_metrics(points: list[dict], now: float) -> list[dict]:
    metrics: dict[str, dict] = {}
    for point in points:
        for field in ("name", "unit", "type", "value"):
            if field not in point:
                raise InputError(f"metric point is missing {field!r}: {point!r}")
        kind = point["type"]
        if kind not in ("gauge", "sum", "histogram"):
            raise InputError(f"{point['name']}: unknown type {kind!r}")
        monotonic = bool(point.get("monotonic", True))
        shape = (point["unit"], kind, monotonic)
        metric = metrics.get(point["name"])
        if metric is None:
            metric = {"name": point["name"], "unit": point["unit"], "_shape": shape}
            if point.get("description"):
                metric["description"] = point["description"]
            if kind == "gauge":
                metric["gauge"] = {"dataPoints": []}
            elif kind == "sum":
                metric["sum"] = {
                    "aggregationTemporality": (
                        AGGREGATION_TEMPORALITY_DELTA
                        if monotonic
                        else AGGREGATION_TEMPORALITY_CUMULATIVE
                    ),
                    "isMonotonic": monotonic,
                    "dataPoints": [],
                }
            else:
                metric["histogram"] = {
                    "aggregationTemporality": AGGREGATION_TEMPORALITY_DELTA,
                    "dataPoints": [],
                }
            metrics[point["name"]] = metric
        elif metric["_shape"] != shape:
            raise InputError(f"{point['name']}: points disagree on unit, type or monotonicity")
        metric[kind]["dataPoints"].append(data_point(point, now))
    for metric in metrics.values():
        del metric["_shape"]
    return list(metrics.values())


def ci_resource(env: dict) -> dict:
    """Upstream `vcs.*` / `cicd.*` resource attributes from the GitHub Actions environment."""
    attrs: dict = {}
    server, repo, run_id = env.get("GITHUB_SERVER_URL"), env.get("GITHUB_REPOSITORY"), env.get("GITHUB_RUN_ID")
    if server and repo:
        attrs["vcs.repository.url.full"] = f"{server}/{repo}"
    for key, var in (
        ("vcs.ref.head.revision", "GITHUB_SHA"),
        ("vcs.ref.head.name", "GITHUB_REF_NAME"),
        ("vcs.ref.head.type", "GITHUB_REF_TYPE"),
        ("cicd.pipeline.name", "GITHUB_WORKFLOW"),
        ("cicd.pipeline.run.id", "GITHUB_RUN_ID"),
    ):
        if env.get(var):
            attrs[key] = env[var]
    if server and repo and run_id:
        attrs["cicd.pipeline.run.url.full"] = f"{server}/{repo}/actions/runs/{run_id}"
    return attrs


def read_cpuinfo_model(path: Path = Path("/proc/cpuinfo")) -> str | None:
    try:
        for line in path.read_text().splitlines():
            key, _, value = line.partition(":")
            if key.strip() == "model name" and value.strip():
                return value.strip()
    except OSError:
        pass
    return None


def read_mem_total_bytes(path: Path = Path("/proc/meminfo")) -> int | None:
    try:
        for line in path.read_text().splitlines():
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) * 1024
    except (OSError, ValueError, IndexError):
        pass
    return None


def host_resource() -> dict:
    """The runner described by spec (`host.*`, `os.type`), never by provider."""
    attrs = {"os.type": platform.system().lower()}
    machine = platform.machine().lower()
    if machine:
        attrs["host.arch"] = HOST_ARCH.get(machine, machine)
    model = read_cpuinfo_model()
    if model:
        attrs["host.cpu.model.name"] = model
    return attrs


def runner_spec_points(now: float) -> list[dict]:
    """Upstream `system.*` readings of the runner's size, as UpDownCounter values."""
    points = []
    cpus = os.cpu_count()
    if cpus:
        points.append(
            {"name": "system.cpu.logical.count", "unit": "{cpu}", "type": "sum",
             "monotonic": False, "value": cpus, "time": now}
        )
    mem = read_mem_total_bytes()
    if mem:
        points.append(
            {"name": "system.memory.limit", "unit": "By", "type": "sum",
             "monotonic": False, "value": mem, "time": now}
        )
    return points


def parse_resource_lines(text: str) -> dict:
    attrs = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        key, sep, value = line.partition("=")
        if not sep or not key.strip():
            raise InputError(f"resource attribute must be key=value, got {line!r}")
        attrs[key.strip()] = value.strip()
    return attrs


def build_payload(resource: dict, metrics: list[dict]) -> dict:
    return {
        "resourceMetrics": [
            {
                "resource": {"attributes": key_values(resource)},
                "scopeMetrics": [{"scope": {"name": SCOPE_NAME}, "metrics": metrics}],
            }
        ]
    }


def post(url: str, token: str, body: bytes, sleep=time.sleep) -> dict:
    request = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
    )
    for attempt in range(1, ATTEMPTS + 1):
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT_S) as response:
                raw = response.read()
                return json.loads(raw) if raw.strip() else {}
        except urllib.error.HTTPError as err:
            if err.code not in RETRYABLE_STATUS or attempt == ATTEMPTS:
                detail = err.read()[:500].decode(errors="replace")
                raise RuntimeError(f"HTTP {err.code} from {url}: {detail}") from None
        except (urllib.error.URLError, TimeoutError) as err:
            if attempt == ATTEMPTS:
                raise RuntimeError(f"cannot reach {url}: {err}") from None
        sleep(2**attempt)
    raise AssertionError("unreachable")


def main(argv: list[str] | None = None, env: dict | None = None) -> int:
    env = dict(os.environ) if env is None else env
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--metrics", type=Path, required=True)
    parser.add_argument("--service-name", required=True)
    parser.add_argument(
        "--resource-attributes",
        default="",
        help="newline-separated key=value pairs, applied over the detected ones",
    )
    parser.add_argument("--runner-spec", action="store_true", help="add host.* and system.* readings")
    parser.add_argument("--dry-run", action="store_true", help="print the payload instead of sending it")
    args = parser.parse_args(argv)

    endpoint = env.get("OTLP_EXPORT_ENDPOINT", "").strip()
    token = env.get("OTLP_EXPORT_TOKEN", "").strip()
    if not args.dry_run and not (endpoint and token):
        print("::notice::OTLP export skipped: endpoint or token is not configured")
        return 0

    now = time.time()
    try:
        points = json.loads(args.metrics.read_text())["metrics"]
        resource = {"service.name": args.service_name, **ci_resource(env)}
        if args.runner_spec:
            resource.update(host_resource())
            points = points + runner_spec_points(now)
        resource.update(parse_resource_lines(args.resource_attributes))
        payload = build_payload(resource, build_metrics(points, now))
    except (OSError, KeyError, ValueError, InputError) as err:
        print(f"::warning::OTLP export: bad input: {err}")
        return 1

    if args.dry_run:
        json.dump(payload, sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 0

    url = endpoint.rstrip("/") + "/v1/metrics"
    try:
        response = post(url, token, json.dumps(payload).encode())
    except RuntimeError as err:
        print(f"::warning::OTLP export failed: {err}")
        return 1
    rejected = response.get("partialSuccess", {}).get("rejectedDataPoints")
    if rejected and int(rejected):
        message = response["partialSuccess"].get("errorMessage", "")
        print(f"::warning::OTLP export: {rejected} data points rejected: {message}")
        return 1
    count = sum(
        len(m[k]["dataPoints"]) for m in payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        for k in ("gauge", "sum", "histogram") if k in m
    )
    print(f"OTLP export: sent {count} data points to {url}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
