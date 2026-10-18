#!/usr/bin/env python3
"""Replay the anonymised corpus as OTLP/JSON logs, forever, at a steady rate.

`just closed-loop-replay` runs this detached so the local Ourios miner sees
real unstructured lines rather than Claude Code's constant event bodies. It
reads every `*.txt` file under testdata/corpus/ (see its README for the
anonymisation notes), sends each line as one log record's string body, and
loops. Standard library only.

The corpus is small, so replayed verbatim it is fully mined in one pass and
every later line is an exact match. Unless --verbatim is given, each line is
varied without adding anything beyond the corpus' own shapes: every number,
UUID and IPv4 token gets a fresh value of the same shape, and a
--mutation-rate fraction of lines either has one word swapped for another
corpus word (near misses: low-confidence parses and widening) or gains a long
base64 parameter (per-parameter overflow and body retention).
"""

import argparse
import base64
import json
import random
import re
import signal
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

RETRY_BACKOFF_SECS = 5.0
DIGIT_RUN = re.compile(r"[0-9]+")
TOKEN = re.compile(r"\S+")
NUM = re.compile(r"-?[0-9]+")
IPV4 = re.compile(r"[0-9]{1,3}(\.[0-9]{1,3}){3}")
UUID = re.compile(r"[0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}")
WORD = re.compile(r"[A-Za-z]{3,}")
BLOB_BYTES = (200, 600)


def load_corpus(corpus_dir):
    lines = []
    for path in sorted(Path(corpus_dir).glob("*.txt")):
        lines.extend(line for line in path.read_text(encoding="utf-8").splitlines() if line)
    return lines


# Only whole tokens the miner masks as parameters (its default NUM, UUID and
# IPv4 rules) get fresh values. Freshening digits inside other tokens, such as
# a `2026/04/02` timestamp or a JSON fragment, would turn every line into a
# new literal and mint one template per line.
def fresh_param(token, rng):
    if UUID.fullmatch(token):
        return "".join(c if c == "-" else rng.choice("0123456789abcdef") for c in token)
    if IPV4.fullmatch(token):
        return ".".join(str(rng.randint(0, 255)) for _ in range(4))
    if NUM.fullmatch(token):
        return DIGIT_RUN.sub(lambda m: "".join(rng.choice("0123456789") for _ in m.group()), token)
    return token


def fresh_params(line, rng):
    return TOKEN.sub(lambda m: fresh_param(m.group(), rng), line)


def swap_word(line, rng, vocabulary):
    tokens = line.split(" ")
    words = [i for i, token in enumerate(tokens) if WORD.fullmatch(token)]
    if not words:
        return line
    tokens[rng.choice(words)] = rng.choice(vocabulary)
    return " ".join(tokens)


def add_blob(line, rng):
    blob = base64.b64encode(rng.randbytes(rng.randint(*BLOB_BYTES))).decode("ascii")
    return f"{line} payload={blob}"


class Varier:
    def __init__(self, lines, mutation_rate, seed):
        self.rng = random.Random(seed)
        self.mutation_rate = mutation_rate
        self.vocabulary = sorted(
            {token for line in lines for token in line.split(" ") if WORD.fullmatch(token)}
        )

    def vary(self, line):
        line = fresh_params(line, self.rng)
        if self.rng.random() >= self.mutation_rate:
            return line
        if self.vocabulary and self.rng.random() < 0.5:
            return swap_word(line, self.rng, self.vocabulary)
        return add_blob(line, self.rng)


def payload(body, service_name):
    now = str(time.time_ns())
    record = {"timeUnixNano": now, "observedTimeUnixNano": now, "body": {"stringValue": body}}
    resource = {"attributes": [{"key": "service.name", "value": {"stringValue": service_name}}]}
    scope_logs = [{"scope": {"name": "corpus-replay"}, "logRecords": [record]}]
    return {"resourceLogs": [{"resource": resource, "scopeLogs": scope_logs}]}


def send(endpoint, body, service_name):
    data = json.dumps(payload(body, service_name)).encode("utf-8")
    request = urllib.request.Request(
        endpoint, data=data, headers={"Content-Type": "application/json"}, method="POST"
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        response.read()


def replay(args):
    lines = load_corpus(args.corpus)
    if not lines:
        sys.exit(f"no corpus lines under {args.corpus}")
    interval = 1.0 / args.rate
    varier = None if args.verbatim else Varier(lines, args.mutation_rate, args.seed)
    mode = "verbatim" if args.verbatim else f"varied, mutation rate {args.mutation_rate}"
    print(f"replaying {len(lines)} lines ({mode}) to {args.endpoint} at {args.rate}/s "
          f"as service.name={args.service_name}", flush=True)
    sent = 0
    next_at = time.monotonic()
    while True:
        for line in lines:
            next_at = max(next_at + interval, time.monotonic())
            body = varier.vary(line) if varier else line
            try:
                send(args.endpoint, body, args.service_name)
                sent += 1
            except (urllib.error.URLError, OSError) as e:
                # The Collector may be restarting; resume the steady rate once it is back.
                print(f"send failed after {sent} lines: {e}; retrying in {RETRY_BACKOFF_SECS}s",
                      flush=True)
                next_at = time.monotonic() + RETRY_BACKOFF_SECS
            time.sleep(max(0.0, next_at - time.monotonic()))
        print(f"corpus pass complete, {sent} lines sent", flush=True)


def positive(value):
    rate = float(value)
    if rate <= 0:
        raise argparse.ArgumentTypeError("rate must be positive")
    return rate


def fraction(value):
    rate = float(value)
    if not 0.0 <= rate <= 1.0:
        raise argparse.ArgumentTypeError("mutation rate must be within [0, 1]")
    return rate


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--endpoint", default="http://127.0.0.1:4318/v1/logs")
    parser.add_argument("--rate", type=positive, default=3.0, help="lines per second")
    parser.add_argument("--service-name", default="corpus-replay")
    parser.add_argument("--corpus", default="testdata/corpus")
    parser.add_argument("--verbatim", action="store_true", help="send corpus lines unchanged")
    parser.add_argument("--mutation-rate", type=fraction, default=0.1,
                        help="fraction of lines given a swapped word or a long parameter")
    parser.add_argument("--seed", type=int, help="seed for reproducible variation")
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        replay(args)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
