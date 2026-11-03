"""Kafka-protocol helpers for the broker-durability spike (Kafka and Redpanda).

  k.py produce BOOT TOPIC N [--label L]   N sequential acks=all produces, one at a time
  k.py count BOOT TOPIC                   records readable from partition 0
  k.py meta BOOT TOPIC [--isr-count N]    leader / replicas / ISR of partition 0
  k.py describe HOST:PORT TOPIC           raw DescribeConfigs v1 sent to that one broker
"""

import json
import socket
import statistics
import struct
import sys
import time

from confluent_kafka import Consumer, Producer, TopicPartition
from confluent_kafka.admin import AdminClient


def produce(boot, topic, n, label):
    p = Producer({
        "bootstrap.servers": boot,
        "acks": "all",
        "linger.ms": 0,
        "enable.idempotence": False,
        "max.in.flight.requests.per.connection": 1,
        "message.timeout.ms": 120000,
        "request.timeout.ms": 60000,
        "retries": 0,
    })
    errors = []

    def one(value):
        box = {}

        def cb(err, msg):
            box["err"] = err
            box["t"] = time.monotonic()
            box["off"] = msg.offset() if err is None else None

        t0 = time.monotonic()
        p.produce(topic, value=value, partition=0, on_delivery=cb)
        p.flush(150)
        if "err" not in box:
            return None, None, "no delivery report"
        if box["err"] is not None:
            return None, None, str(box["err"])
        return (box["t"] - t0) * 1000.0, box["off"], None

    _, _, err = one(b"warmup")  # metadata + connection setup off the clock
    if err:
        print(f"WARMUP ERROR {err}", file=sys.stderr)
    lats, offs = [], []
    for i in range(n):
        ms, off, err = one(f"{label}-{i}".encode())
        if err:
            errors.append(err)
            print(f"  produce {i}: ERROR {err}")
            continue
        lats.append(ms)
        offs.append(off)
        print(f"  produce {i}: {ms:8.1f} ms  offset {off}")
    out = {
        "label": label,
        "topic": topic,
        "acked": len(lats),
        "errors": errors,
        "offsets": offs,
        "lat_ms": [round(x, 1) for x in lats],
        "min_ms": round(min(lats), 1) if lats else None,
        "median_ms": round(statistics.median(lats), 1) if lats else None,
        "max_ms": round(max(lats), 1) if lats else None,
    }
    print("RESULT " + json.dumps(out))
    return out


def count(boot, topic, timeout_s=20):
    c = Consumer({
        "bootstrap.servers": boot,
        "group.id": f"spike-count-{time.time_ns()}",
        "enable.auto.commit": False,
        "enable.partition.eof": True,
        "auto.offset.reset": "earliest",
        "socket.timeout.ms": 10000,
    })
    tp = TopicPartition(topic, 0)
    try:
        lo, hi = c.get_watermark_offsets(tp, timeout=timeout_s)
    except Exception as e:  # partition offline or no leader
        print("COUNT " + json.dumps({"topic": topic, "error": str(e)}))
        c.close()
        return None
    c.assign([TopicPartition(topic, 0, lo)])
    n, values, deadline = 0, [], time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        m = c.poll(1.0)
        if m is None:
            continue
        if m.error():
            if m.error().code() == -191:  # _PARTITION_EOF
                break
            print(f"consume error {m.error()}", file=sys.stderr)
            continue
        n += 1
        values.append(m.value().decode(errors="replace"))
    c.close()
    out = {"topic": topic, "low": lo, "high": hi, "read": n,
           "first": values[:2], "last": values[-2:]}
    print("COUNT " + json.dumps(out))
    return out


def meta(boot, topic, isr_count=None):
    a = AdminClient({"bootstrap.servers": boot, "socket.timeout.ms": 5000})
    md = a.list_topics(topic, timeout=10)
    t = md.topics[topic]
    if t.error is not None:
        print("META " + json.dumps({"topic": topic, "error": str(t.error)}))
        sys.exit(1)
    p = t.partitions[0]
    out = {"topic": topic, "leader": p.leader, "replicas": p.replicas, "isr": p.isrs,
           "error": str(p.error) if p.error else None}
    print("META " + json.dumps(out))
    if isr_count is not None and len(p.isrs) != isr_count:
        sys.exit(1)


SOURCES = {0: "UNKNOWN", 1: "TOPIC_CONFIG", 2: "DYNAMIC_BROKER_CONFIG",
           3: "DYNAMIC_DEFAULT_BROKER_CONFIG", 4: "STATIC_BROKER_CONFIG",
           5: "DEFAULT_CONFIG", 6: "DYNAMIC_BROKER_LOGGER_CONFIG"}


class R:
    def __init__(self, b):
        self.b, self.o = b, 0

    def take(self, fmt):
        v = struct.unpack_from(fmt, self.b, self.o)
        self.o += struct.calcsize(fmt)
        return v[0]

    def s(self):
        n = self.take(">h")
        if n < 0:
            return None
        v = self.b[self.o:self.o + n].decode()
        self.o += n
        return v


def _str(s):
    b = s.encode()
    return struct.pack(">h", len(b)) + b


def describe(hostport, topic):
    """DescribeConfigs v1 with include_synonyms, to exactly this broker."""
    host, port = hostport.rsplit(":", 1)
    body = struct.pack(">i", 1) + struct.pack(">b", 2) + _str(topic) + struct.pack(">i", -1)
    body += struct.pack(">?", True)
    header = struct.pack(">hhi", 32, 1, 7) + _str("ourios-spike")
    req = header + body
    with socket.create_connection((host, int(port)), timeout=10) as s:
        s.sendall(struct.pack(">i", len(req)) + req)
        size = struct.unpack(">i", _recv(s, 4))[0]
        r = R(_recv(s, size))
    r.take(">i")  # correlation id
    r.take(">i")  # throttle
    out = {}
    for _ in range(r.take(">i")):
        err, msg = r.take(">h"), r.s()
        r.take(">b")
        r.s()
        if err:
            print(f"DESCRIBE {hostport} error {err} {msg}")
        for _ in range(r.take(">i")):
            name, value = r.s(), r.s()
            r.take(">?")
            src = r.take(">b")
            r.take(">?")
            syn = []
            for _ in range(r.take(">i")):
                sn, sv, ss = r.s(), r.s(), r.take(">b")
                syn.append([sn, sv, SOURCES.get(ss, ss)])
            out[name] = {"value": value, "source": SOURCES.get(src, src), "synonyms": syn}
    print("DESCRIBE " + json.dumps({"broker": hostport, "topic": topic, "configs": out}))
    return out


def _recv(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def main(argv):
    cmd = argv[1]
    if cmd == "produce":
        label = argv[argv.index("--label") + 1] if "--label" in argv else "rec"
        produce(argv[2], argv[3], int(argv[4]), label)
    elif cmd == "count":
        count(argv[2], argv[3])
    elif cmd == "meta":
        isr = int(argv[argv.index("--isr-count") + 1]) if "--isr-count" in argv else None
        meta(argv[2], argv[3], isr)
    elif cmd == "describe":
        describe(argv[2], argv[3])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
