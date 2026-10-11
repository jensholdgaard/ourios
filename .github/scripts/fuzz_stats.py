#!/usr/bin/env python3
"""Turn one smoke-fuzz run's libFuzzer log into otlp-export metric points.

Reads the log `fuzz.yml` tees from `cargo fuzz run ... -print_final_stats=1`
and prints the JSON the `.github/actions/otlp-export` action consumes. The
metric and attribute names are registered in ourios-semconv
(`ourios.fuzz.*`, #942). Standard library only.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
from pathlib import Path

STATUS_LINE = re.compile(
    r"^#(?P<iter>\d+)\s+\w+\s+cov: (?P<cov>\d+) ft: (?P<ft>\d+) "
    r"corp: (?P<inputs>\d+)/(?P<size>\d+)(?P<unit>b|Kb|Mb|Gb)\b"
)
FINAL_STAT = re.compile(r"^stat::(?P<key>\w+):\s+(?P<value>\d+)\s*$")
CORPUS_UNIT_BYTES = {"b": 1, "Kb": 1 << 10, "Mb": 1 << 20, "Gb": 1 << 30}

# Checked in order; the first marker found names the finding. Leak comes
# before the generic sanitizer marker because LeakSanitizer's report also
# matches it.
FINDING_MARKERS = (
    ("timeout", re.compile(r"ERROR: libFuzzer: timeout after")),
    ("oom", re.compile(r"ERROR: libFuzzer: out-of-memory")),
    ("leak", re.compile(r"ERROR: LeakSanitizer")),
    (
        "crash",
        re.compile(
            r"ERROR: libFuzzer: |ERROR: \w+Sanitizer|"
            r"SUMMARY: libFuzzer: |panicked at "
        ),
    ),
)


def parse_log(text: str) -> dict:
    """Extract the last status line and the final stats from a libFuzzer log."""
    status = None
    stats: dict[str, int] = {}
    for line in text.splitlines():
        match = STATUS_LINE.match(line)
        if match:
            status = match
            continue
        match = FINAL_STAT.match(line)
        if match:
            stats[match["key"]] = int(match["value"])

    parsed: dict[str, int] = {}
    if status:
        parsed["coverage"] = int(status["cov"])
        parsed["features"] = int(status["ft"])
        parsed["corpus_inputs"] = int(status["inputs"])
        parsed["corpus_bytes"] = int(status["size"]) * CORPUS_UNIT_BYTES[status["unit"]]
    if "number_of_executed_units" in stats:
        parsed["executions"] = stats["number_of_executed_units"]
    if "average_exec_per_sec" in stats:
        parsed["exec_per_sec"] = stats["average_exec_per_sec"]
    if "peak_rss_mb" in stats:
        # libFuzzer's "Mb" is ru_maxrss >> 10, i.e. MiB.
        parsed["peak_rss_bytes"] = stats["peak_rss_mb"] << 20
    return parsed


def classify(text: str, step_outcome: str) -> str:
    """Map the run step's outcome plus its log to an `ourios.fuzz.result` value."""
    if step_outcome == "success":
        return "ok"
    if step_outcome == "cancelled":
        return "cancelled"
    for result, marker in FINDING_MARKERS:
        if marker.search(text):
            return result
    return "error"


def build_points(
    *,
    target: str,
    result: str,
    parsed: dict,
    budget: float,
    start: float | None,
    end: float | None,
    now: float,
) -> list[dict]:
    attrs = {"ourios.fuzz.target": target, "ourios.fuzz.result": result}
    timing = {"time": end if end is not None else now}
    if start is not None:
        timing["start_time"] = start

    def point(name: str, unit: str, kind: str, value, attributes=attrs) -> dict:
        return {
            "name": name,
            "unit": unit,
            "type": kind,
            "value": value,
            "attributes": dict(attributes),
            **timing,
        }

    points = [
        point("ourios.fuzz.runs", "{run}", "sum", 1),
        point(
            "ourios.fuzz.run.budget",
            "s",
            "gauge",
            budget,
            {"ourios.fuzz.target": target},
        ),
    ]
    if start is not None and end is not None and end >= start:
        points.append(point("ourios.fuzz.run.duration", "s", "histogram", end - start))
    optional = (
        ("coverage", "ourios.fuzz.coverage", "{edge}", "gauge"),
        ("features", "ourios.fuzz.feature.count", "{feature}", "gauge"),
        ("corpus_inputs", "ourios.fuzz.corpus.input.count", "{input}", "gauge"),
        ("corpus_bytes", "ourios.fuzz.corpus.size", "By", "gauge"),
        ("executions", "ourios.fuzz.executions", "{execution}", "sum"),
        ("exec_per_sec", "ourios.fuzz.execution.rate", "{execution}/s", "gauge"),
        ("peak_rss_bytes", "ourios.fuzz.memory.peak", "By", "gauge"),
    )
    for key, name, unit, kind in optional:
        if key in parsed:
            points.append(point(name, unit, kind, parsed[key]))
    return points


def optional_epoch(raw: str) -> float | None:
    raw = raw.strip()
    return float(raw) if raw else None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--log", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument(
        "--step-outcome",
        required=True,
        help="steps.<id>.outcome of the run step: success, failure, cancelled or skipped",
    )
    parser.add_argument("--budget", type=float, required=True, help="-max_total_time, seconds")
    parser.add_argument("--start", default="", help="run start, Unix epoch seconds")
    parser.add_argument("--end", default="", help="run end, Unix epoch seconds")
    args = parser.parse_args(argv)

    text = args.log.read_text(errors="replace") if args.log.is_file() else ""
    points = build_points(
        target=args.target,
        result=classify(text, args.step_outcome),
        parsed=parse_log(text),
        budget=args.budget,
        start=optional_epoch(args.start),
        end=optional_epoch(args.end),
        now=time.time(),
    )
    json.dump({"metrics": points}, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
