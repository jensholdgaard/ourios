#!/usr/bin/env python3
"""Flatten an Ourios /v1/query response into the JSON lines Gonzo parses.

Ourios returns OTLP-faithful records (attributes as `{key, value:{...}}`
arrays, nanosecond timestamps, a structured `body`). Gonzo auto-detects
flat JSON with a timestamp, a level and a message, so this is the adapter
between the two — read the query response on stdin, write one object per
line on stdout.
"""
import json
import signal
import sys

# OTLP severity number → the level names Gonzo colours.
SEVERITY = [
    (1, 4, "TRACE"), (5, 8, "DEBUG"), (9, 12, "INFO"),
    (13, 16, "WARN"), (17, 20, "ERROR"), (21, 24, "FATAL"),
]


def level(number: int) -> str:
    for low, high, name in SEVERITY:
        if low <= number <= high:
            return name
    return "INFO"


def scalar(value: dict):
    """The one populated member of an OTLP AnyValue, as a plain scalar."""
    if not isinstance(value, dict):
        return value
    for key in ("stringValue", "boolValue", "bytesValue"):
        if key in value:
            return value[key]
    for key in ("intValue", "doubleValue"):
        if key in value:
            return value[key]
    if "arrayValue" in value or "kvlistValue" in value:
        return json.dumps(value)
    return None


def message(record: dict) -> str:
    body = record.get("body")
    if isinstance(body, dict):
        # RFC 0043: a rendered body carries `line`; a structured one is
        # the value itself.
        if "line" in body:
            return body["line"]
        return json.dumps(body.get("value", body))
    if body is None:
        return record.get("event_name") or f"template {record.get('template_id')}"
    return str(body)


def flatten(record: dict) -> dict:
    out = {
        "timestamp": record["time_unix_nano"] / 1e9,
        "level": record.get("severity_text") or level(record.get("severity_number", 9)),
        "message": message(record),
    }
    for scope, key in (("attributes", "attr"), ("resource_attributes", "resource")):
        for kv in record.get(scope) or []:
            value = scalar(kv.get("value"))
            if value is not None:
                out[f"{key}.{kv['key']}"] = value
    for key in ("scope_name", "template_id", "event_name"):
        if record.get(key) is not None:
            out[key] = record[key]
    return out


def main() -> int:
    # The consumer is a TUI the operator quits at will; a closed pipe is a
    # normal end, not a traceback.
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    payload = json.load(sys.stdin)
    records = payload.get("records", [])
    if not records:
        print(
            f"no rows for that query (the response reported {payload.get('rows', 0)} "
            "matching rows before limits)",
            file=sys.stderr,
        )
    for record in records:
        print(json.dumps(flatten(record)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
