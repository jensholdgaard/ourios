# OTLP log format — what crosses the wire vs. what Ourios consumes today

> Status: **investigation finding**. Drafted 2026-05-13 to answer
> "is our template miner targeting the actual OTLP shape, or a
> made-up one?" Conclusion: the latter. This doc surfaces the gap
> and lists the RFC patches it implies; it does not change code.

The Ourios glossary commits the project's ingest contract to
**OTLP over gRPC and HTTP** — "we do not invent our own format"
([`docs/glossary.md`](../glossary.md), entry **OTLP**). The
template-miner RFC ([`docs/rfcs/0001-template-miner.md`](../rfcs/0001-template-miner.md))
does not carry through on that commitment: §6.1's record schema
has eight fields, none of which exist on the OTLP wire, and the
ingest signature today is `MinerCluster::ingest(tenant_id, raw:
&str)` — a flat text line, not a structured `LogRecord`. This
document closes the loop.

The first audience for this finding is the maintainer; the
second is the RFC 0001 amendment PR and the future RFC 0003
(OTLP receiver) it implies.

---

## 1. What OTLP actually carries

The wire-level definition lives in
`opentelemetry-proto/opentelemetry/proto/logs/v1/logs.proto` and
the spec at
[opentelemetry.io/docs/specs/otel/logs/data-model](https://opentelemetry.io/docs/specs/otel/logs/data-model/).
The relevant message hierarchy is:

```
LogsData
└── ResourceLogs[]
    ├── resource: Resource           ← Resource.attributes carries service.name, host.*, etc.
    ├── schema_url: string
    └── scope_logs: ScopeLogs[]
        ├── scope: InstrumentationScope   ← name, version, attributes
        ├── schema_url: string
        └── log_records: LogRecord[]
```

A single **`LogRecord`** carries:

| Field | Type | Notes |
|---|---|---|
| `time_unix_nano` | `fixed64` | Event time at the source; `0` = unknown |
| `observed_time_unix_nano` | `fixed64` | When the collector saw it; required once observed |
| `severity_number` | `enum` | Normalised TRACE..FATAL with sub-levels (1–24) |
| `severity_text` | `string` | Source's original level string |
| `body` | `AnyValue` | **The log content. Not necessarily a string.** |
| `attributes` | `KeyValue[]` | Per-occurrence structured context |
| `dropped_attributes_count` | `uint32` | Truncation indicator |
| `flags` | `fixed32` | Lower 8 bits = W3C trace flags |
| `trace_id` | `bytes` (16) | Trace correlation |
| `span_id` | `bytes` (8) | Span correlation |
| `event_name` | `string` | Identifier for structured-event records |

Plus, inherited from the parent containers: the **`Resource`**
attributes (the unit of "where did this come from" — typically
`service.name`, `host.name`, `k8s.pod.uid`, etc.) and the
**`InstrumentationScope`** name/version (which library/module
emitted this record).

**`AnyValue` is a `oneof` of:** `string_value`, `bool_value`,
`int_value`, `double_value`, `array_value` (recursive),
`kvlist_value` (recursive map of strings → AnyValue), and
`bytes_value`. The spec is explicit about the structured case:

> Body MUST support AnyValue to preserve the semantics of
> structured logs emitted by the applications.

So a real OTLP emitter is at liberty to send a `LogRecord` whose
`body` is, for example, `{"msg": "user logged in", "user_id": 42,
"from_ip": "10.0.0.1"}` as a `kvlist_value` — with the parameters
already structured out, not embedded in a free-text string.

---

## 2. What Ourios consumes today

`MinerCluster::ingest(tenant_id: &TenantId, raw: &str) -> u64`
([`crates/ourios-miner/src/cluster.rs`](../../crates/ourios-miner/src/cluster.rs)).
The pipeline:

1. `tokenize(raw)` splits on Unicode whitespace
   ([`tokenize.rs`](../../crates/ourios-miner/src/tokenize.rs)).
2. `mask(tokens)` runs UUID / IPv4 / NUM rules over the resulting
   `&str` slice ([`mask.rs`](../../crates/ourios-miner/src/mask.rs)).
3. `descend` + leaf lookup attaches to or creates a template
   ([`tree.rs`](../../crates/ourios-miner/src/tree.rs)).

The Parquet record promised by RFC 0001 §6.1 carries:

```
tenant_id, template_id, template_version, params,
separators, body?, confidence, lossy_flag
```

That's the entire data model. **Zero fields from the OTLP wire
are reflected in the record.**

---

## 3. The gaps

### 3.1 Severity is missing from the record (and from the template key)

`severity_number` is one of the most common operator query
filters: "show me all ERROR-or-worse from `service.name = api`
in the last hour." Today the miner has no severity field.

Worse: the template *key* doesn't include severity. A line emitted
at INFO and the same line emitted at ERROR would currently
collapse to one `template_id`. That's a §3.1-class problem
("no silent template merges") in disguise — two semantically
distinct events sharing one id.

### 3.2 Timestamps are missing from the record

`time_unix_nano` and `observed_time_unix_nano` carry the data
that the **B1 thesis gate** ("predicate-pushdown query latency on
time/template/tenant filters") explicitly measures. Without a
time column we cannot run B1 at all.

Today there is no time field on the record. The Parquet writer
PR (Phase 2 in [`docs/roadmap.md`](../roadmap.md)) cannot land
without RFC 0001 §6.1 amending to add at least
`time_unix_nano`.

### 3.3 Resource and scope are missing

`Resource.attributes` is OTLP's "who sent this" partition key —
in real deployments, `service.name` is the natural partition for
template trees (it's effectively the per-service template
namespace). Today our `tenant_id` is operator-supplied and has no
declared mapping from OTLP fields. We need to decide:
`tenant_id := resource.attributes["service.name"]`? Or some
configured mapping rule? RFC 0003 (OTLP receiver) is the place
for this; RFC 0001 just needs to make `resource_attributes` a
record column so the decision can land.

`InstrumentationScope.name` distinguishes the same body text
emitted from different code paths in the same service. Likely
also belongs in the template key — `myapp.login` and
`myapp.checkout` emitting `"request received"` are different
events.

### 3.4 Attributes carry the structured params we try to mine

In a structured-logging world, the values our `mask()` rules try
to extract from text (NUMs, IPs, UUIDs) are typically **already
typed and separated** by the SDK as Attributes. A modern
emitter sends:

- `body = "user logged in"`
- `attributes = {"user.id": 42, "client.address": "10.0.0.1"}`

Not:

- `body = "user 42 logged in from 10.0.0.1"`
- `attributes = {}`

Our miner gets the second form and does work to reconstruct
roughly what the first form already had. Worse, given the first
form, we currently mine `"user logged in"` as a flat fixed
template and **lose the typed attribute values entirely** —
they'd never reach the Parquet record. The operator query "show
me all logins from `client.address = 10.0.0.1`" returns nothing.

The implication for the miner is significant: the `params` slot
on the record cannot be only "things `mask()` extracted from the
body string." It must also carry the OTLP `attributes` of the
record — either as a sibling column (operator-queryable) or
folded into the existing `params` shape (more complex).

### 3.5 Body is not always a string

`AnyValue` body. Today `ingest(raw: &str)` cannot accept a
structured body at all. Three plausible paths:

1. **Render-to-string at the receiver.** Convert structured Body
   to a canonical JSON-ish string before handing to the miner.
   Loses the structure but preserves the existing miner shape.
   Risk: §3.3 ("bit-identical body reconstruction") requires
   the rendered form to round-trip; canonicalising arbitrary
   AnyValue trees is non-trivial.
2. **Treat structured Body as not-mineable.** Store it verbatim
   in the `body?` column with `lossy_flag = false` (it's an
   explicit structured value, not a lossy reconstruction); the
   miner emits a `template_id` of "structured body" and the
   query path knows to read `body?` directly. Simpler, gives up
   templating for those records.
3. **Mine inner string fields.** If `body` is a `kvlist_value`
   with a `"msg"` field, mine `msg` as the line. Pragmatic but
   ad-hoc; the field name is convention not spec.

Path (2) is the cleanest minimum; path (1) is the eventual
ambition; path (3) is a configurable convenience layer. **All
three need an explicit spec decision.**

### 3.6 Trace correlation is missing

`trace_id`, `span_id`, `flags` are how operators correlate logs
to spans in the same trace. Real operators use this constantly.
Today: no fields, no support. Add as record columns.

### 3.7 The ingest signature itself is wrong

`ingest(tenant_id: &TenantId, raw: &str)` cannot accept any
of the above. The eventual signature is roughly:

```rust
fn ingest(&mut self, record: &OtlpLogRecord) -> u64
```

…where `OtlpLogRecord` is a struct that mirrors the OTLP wire
shape (or borrows directly from a `tonic`-decoded protobuf
message). This is a breaking change to the cluster's public
surface and is rightly the territory of RFC 0001's amendment.

---

## 4. Implications

### 4.1 RFC 0001 §6.1 needs amendment

The minimum schema additions to make the record OTLP-faithful:

| Add | Type | Rationale |
|---|---|---|
| `time_unix_nano` | `u64` | B1 gate; required column |
| `observed_time_unix_nano` | `Option<u64>` | OTLP has both |
| `severity_number` | `u8` | Operator queries; template key |
| `severity_text` | `Option<String>` | Source's original level |
| `attributes` | `KeyValue[]` | The structured params we currently miss |
| `resource_attributes` | `KeyValue[]` | `service.name` etc. |
| `scope_name` | `Option<String>` | Template-key candidate |
| `scope_version` | `Option<String>` | Diagnostic / drift detection |
| `trace_id` | `Option<[u8; 16]>` | Trace correlation |
| `span_id` | `Option<[u8; 8]>` | Trace correlation |
| `flags` | `u32` | W3C trace flags |
| `event_name` | `Option<String>` | Structured-event records |

Plus an explicit decision on:

- **Template key.** Is the leaf identified by
  `(masked_body_tokens)` alone, or by some tuple of
  `(severity_number, scope_name, masked_body_tokens)`?
- **`body` representation.** AnyValue → what does the miner see?
  (Per §3.5 above.)
- **`tenant_id` derivation.** What OTLP field(s) define it?

### 4.2 RFC 0001 §6.2 (algorithm) needs a tokenize/mask amendment

`tokenize` + `mask` are designed for text. Once Body is AnyValue,
the front of the pipeline branches: structured Body skips the
tokenize/mask path entirely (or uses path (3) above on a
configured field). The algorithm spec needs to acknowledge this
fork.

### 4.3 RFC 0003 (OTLP receiver) becomes a prerequisite, not a follow-up

Today's `roadmap.md` §5 lists the OTLP receiver as
"first post-MVP shipping PR series." That sequencing assumes the
receiver is just the wire-decode-and-forward layer for an
already-OTel-aligned record schema. With the gaps in §3 above,
the receiver and the schema co-evolve: you cannot define the
record without knowing what the receiver hands you, and you
cannot define the receiver without knowing what the record
expects. **RFC 0003 should be drafted alongside the RFC 0001
amendment, not after it.**

### 4.4 The Phase-3 corpus + bench need an OTLP-shaped corpus

The corpus runner (`ourios-bench`, Phase 3) cannot validly
exercise the C2 thesis gate (template-count convergence) on
flat-text input if the production input is OTLP. The corpus
input must itself be OTLP-shaped — either a pre-recorded
batch of `LogsData` protobuf, or a generator that emits
realistic `LogRecord`s including the structured-Body and
attributes-bearing variants.

### 4.5 The current cluster's behaviour is not fully wrong, just narrow

Plain-text traditional logs (Syslog, Log4j, slog with default
text formatter) produce `LogRecord`s with string Body and
near-empty Attributes. The current miner handles those records
correctly modulo the missing timestamp / severity / resource
columns. So the current code is not throw-away; it's the **text
arm of a fork** that the OTLP-aware ingest will need.

---

## 5. Recommendation

Three follow-ups, in order:

1. **Patch RFC 0001 §6.1 + §6.2** (a `meta:`-shaped change to
   the record schema and the algorithm spec). Land the new
   columns, the template-key decision, and the AnyValue
   handling fork. Do this first because the rest of the work
   depends on it.

2. **Draft RFC 0003 — OTLP receiver.** Cover (a) the
   wire-decode layer (`tonic` for gRPC, `axum`/`hyper` for
   HTTP/protobuf, against the official `opentelemetry-proto`
   crate); (b) the `OtlpLogRecord → MinerCluster` mapping; (c)
   the `tenant_id` derivation rule; (d) the WAL-before-ack
   sequencing under the new structured shape (§3.4); (e)
   build-vs-depend evaluation (`tonic` + hand-roll vs. embedding
   the `rotel` Rust collector vs. running the OTel Collector
   out-of-process and forwarding).

3. **Patch the miner crates** to consume the new record shape
   and route Body through the AnyValue fork. Update the
   roadmap to reflect that OTel-native ingest is no longer
   strictly post-MVP for the C2 gate's validity.

The user-visible effect: the eventual benchmarks measure what an
actual OTel deployment would experience, not a flat-text
caricature of it. The thesis claim of "Parquet + template mining
+ DataFusion is the right stack for OTel logs" becomes testable
in the form an operator would actually evaluate it.

---

## 6. References

- OTLP `logs.proto`:
  [github.com/open-telemetry/opentelemetry-proto/blob/main/opentelemetry/proto/logs/v1/logs.proto](https://github.com/open-telemetry/opentelemetry-proto/blob/main/opentelemetry/proto/logs/v1/logs.proto)
- OTLP `common.proto` (AnyValue, KeyValue):
  [github.com/open-telemetry/opentelemetry-proto/blob/main/opentelemetry/proto/common/v1/common.proto](https://github.com/open-telemetry/opentelemetry-proto/blob/main/opentelemetry/proto/common/v1/common.proto)
- OpenTelemetry Logs Data Model spec:
  [opentelemetry.io/docs/specs/otel/logs/data-model](https://opentelemetry.io/docs/specs/otel/logs/data-model/)
- RFC 0001 §6.1 (current record schema):
  [`docs/rfcs/0001-template-miner.md`](../rfcs/0001-template-miner.md)
- Glossary entry **OTLP** (the load-bearing commitment):
  [`docs/glossary.md`](../glossary.md)

*Last updated: 2026-05-13.*
