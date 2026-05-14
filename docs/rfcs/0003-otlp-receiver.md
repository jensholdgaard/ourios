---
rfc: 0003
title: OTLP receiver — gRPC and HTTP wire endpoints for OpenTelemetry log ingest
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-05-13
supersedes: —
superseded-by: —
---

# RFC 0003 — OTLP receiver

> **How to read this document.** Sections §§1–4 are the design
> contract — the *what* and the *why*. §5 lists the normative
> `Given / When / Then` scenarios — the contract — but is a stub
> at this `drafted` stage and will be filled to move the RFC to
> `specified`. §6 is the precise specification the receiver
> crate is implemented against. §7 records the alternatives we
> evaluated and rejected. §8 maps each §5 scenario (when filled)
> to the technique that tests it. §9 lists open questions; §10
> the references.
>
> Cross-references to `CLAUDE.md` sections are in square
> brackets, e.g. `[§3.4]`, and name the invariant the section
> must preserve. Cross-references to RFC 0001 use its section
> numbers directly (e.g. *RFC 0001 §6.1*).

## 1. Summary

The Ourios OTLP receiver accepts OpenTelemetry log batches over
gRPC and HTTP, decodes them per the official `opentelemetry-proto`
schema, derives a `tenant_id` per `ResourceLogs` group via an
operator-configured rule (RFC 0001 §6.1 *Tenant derivation*),
canonicalises every `LogRecord.body` whose
`body.kind != AnyValue::String` to its OTLP-canonical JSON
encoding (string bodies pass through as `L_raw` per RFC 0001
§6.2 step 0), fans the batch out into per-tenant streams of
`OtlpLogRecord`, hands each stream to `ourios-miner`, and
acknowledges the OTLP request only after the WAL-before-ack
invariant `[§3.4]` is satisfied. The default wire stack is
`tonic` (gRPC) + `axum`/`hyper` (HTTP) against the official
`opentelemetry-proto` Rust crate; the alternatives considered and
rejected are embedding `rotel` as a library and running the OTel
Collector out-of-process.

This RFC is the wire-decode contract that the §6.1 amendment of
RFC 0001 (PR #21) and the §6.2 algorithm rewrite (PR #23) both
implicitly require: the miner takes a structured `OtlpLogRecord`
that *something* must produce. RFC 0003 is that something.

## 2. Motivation

### 2.1 The OTel-native commitment is not yet implemented

`docs/glossary.md` (entry **OTLP**) commits Ourios to OTLP as the
sole ingest contract: *"we do not invent our own format."* RFC
0001's pre-amendment §6.1 record schema and the
`MinerCluster::ingest(_, raw: &str)` signature treated logs as
flat text strings, which the investigation in
`docs/architecture/otlp-log-format.md` (PR #20) showed to be
incompatible with that commitment. PRs #21 and #23 amended the
miner's data model and algorithm to consume structured
`OtlpLogRecord`s. **No code yet produces those records.** This
RFC specifies the producer.

### 2.2 The receiver is the boundary that decides what "OTLP" means in practice

OTLP carries a structured `LogRecord` whose `body` is
`AnyValue` (string, bool, int, double, bytes, array, kvlist),
whose attributes are typed, whose `Resource` lives one container
level up (per `ResourceLogs`), and whose timestamps and severity
are first-class. Where in the pipeline these wire-level facts
become the in-memory `OtlpLogRecord` the miner sees is a
load-bearing decision: the receiver is where:

- The wire format (protobuf vs JSON, gRPC vs HTTP) collapses to
  a single in-memory representation.
- `tenant_id` is derived per `ResourceLogs` (RFC 0001 §6.1
  *Tenant derivation*) and the batch fans out into per-tenant
  streams.
- `body.kind = Structured` records have their `AnyValue` body
  canonicalised to OTLP-canonical JSON (RFC 0001 §6.1 *Body
  representation*) so the round-trip
  `stored_bytes ↔ AnyValue` is well-defined and the
  `lossy_flag = false` promise is meetable.
- The acknowledgement-after-durability sequencing (`[§3.4]`)
  is enforced.

Specifying these decisions in one place — and pinning them
explicitly against the OTel spec rather than reinventing them —
is what this RFC does.

### 2.3 Roadmap context

`docs/roadmap.md` §5 (post-#22) lists "OTLP wire endpoints
(gRPC + HTTP listeners)" as **post-MVP**: the bench reads OTLP
from disk (a corpus of pre-recorded `LogsData`), not from the
network, so wire-decode is not on the C2 thesis-gate path. The
**record shape** is in MVP — the miner consumes `OtlpLogRecord`
from the corpus reader. This RFC is the spec for the
post-MVP wire layer; landing the spec now (rather than after
the bench) settles the design while the OTLP record shape is
being implemented in the miner, so the receiver's eventual
implementation has nothing to redesign.

## 3. Background — OTLP wire formats

### 3.1 The OTLP message hierarchy

An OTLP log export is a single `ExportLogsServiceRequest`
message carrying one or more `ResourceLogs`. (`LogsData` is the
file-format equivalent message in `logs.proto` and shares the
same `resource_logs: ResourceLogs[]` field shape; this RFC uses
`ExportLogsServiceRequest` throughout, since that is the wire
type both transports decode into.)

```
ExportLogsServiceRequest
└── resource_logs: ResourceLogs[]
    ├── resource: Resource           # per-source attributes (service.name, host.*, ...)
    ├── schema_url: string
    └── scope_logs: ScopeLogs[]
        ├── scope: InstrumentationScope     # name, version, attributes
        ├── schema_url: string
        └── log_records: LogRecord[]        # the actual log entries
```

A single export request can carry records from multiple
sources (multiple `ResourceLogs` groups), each with its own
`Resource`, and within each Resource multiple instrumentation
scopes. The mapping from this hierarchy to per-tenant streams
of records is the receiver's responsibility (§6.4 below).

### 3.2 Two transports, three encodings

OTLP is defined for two transports:

- **OTLP/gRPC** — the canonical transport. Service is
  `opentelemetry.proto.collector.logs.v1.LogsService`, method
  `Export`. Wire encoding: protobuf over HTTP/2.
- **OTLP/HTTP** — POST against the `/v1/logs` path. Wire
  encoding chosen by the client per the `Content-Type` header:
  - `application/x-protobuf` (recommended by the spec; the
    same protobuf message as gRPC)
  - `application/json` (the proto3 JSON mapping with OTLP
    overrides — hex `trace_id`/`span_id`, base64 `bytes`)

The receiver MUST support both transports and all three
encodings. The OTel emitter ecosystem is split: SDKs ship with
gRPC by default, but the HTTP transport is widely used in
constrained environments and as a Collector exporter. Refusing
either transport reduces the receiver to a non-compliant subset.

### 3.3 Backpressure and partial-success in the OTLP response

The OTLP spec defines a partial-success response shape:

```
ExportLogsServiceResponse
└── partial_success: ExportLogsPartialSuccess (optional)
    ├── rejected_log_records: int64
    └── error_message: string
```

When set, this signals that the receiver accepted *some* records
but rejected others (e.g., due to rate limiting, validation
failures, etc.). The full-failure case uses a transport-level
error (gRPC status code, HTTP non-2xx) rather than
partial-success.

§6.7 below discusses how Ourios uses (or defers using) this
field. For the initial design, the receiver uses **all-or-
nothing** batch semantics — full-success or transport-level
error — and reserves partial-success for a future RFC.

## 4. Background — Existing Rust OpenTelemetry ecosystem

### 4.1 `opentelemetry-proto`

The official Rust crate carrying generated bindings for every
`opentelemetry-proto` message. Tracks upstream proto. Suitable
as the in-memory representation between wire-decode and the
miner. Trivially compatible with both `tonic` (gRPC) and
`prost` (raw protobuf, used over HTTP).

### 4.2 `tonic`

The de-facto Rust gRPC framework: production-grade, async
(tokio), supports the metadata, deadlines, and streaming
features OTLP relies on. Standard choice for any Rust gRPC
service. The `LogsService` server trait is generated by the
`tonic-build` codegen step from the OTLP `.proto` files.

### 4.3 `axum` and `hyper`

`axum` is the conventional Rust HTTP framework for service
endpoints, built on `hyper` and `tower`. Suitable for the OTLP/
HTTP transport. The endpoint handler decodes the request body
(protobuf or JSON, dispatched on `Content-Type`) into the same
in-memory `ExportLogsServiceRequest` representation `tonic`
produces, and the two transport paths converge into a single
business-logic layer.

### 4.4 `rotel`

A Rust-implemented OpenTelemetry Collector. Mature (production
deployments exist), covers receivers, processors, exporters.
Interesting as a possible *library* to embed (taking just its
OTLP-receiver component) rather than as a separate process —
see §7 for the build-vs-embed analysis. Note: `rotel`'s public
API is collector-shaped (full pipeline), not "just the receiver"
shaped, which complicates embedding.

### 4.5 OTel Collector (Go)

The reference collector implementation. Often deployed as a
sidecar or daemonset that buffers, batches, and forwards
telemetry to backends. For an Ourios deployment, a fronting
Collector would terminate OTLP at the Collector and forward to
Ourios via some other transport (or via OTLP again). §7
discusses this as a deployment option, not a code dependency.

## 5. Acceptance criteria

> **Stub.** This RFC is at status `drafted`. §5 acceptance
> criteria are filled to move to `specified`. The list below
> sketches the scenarios that will be specified — actual
> Given/When/Then text is deliberately deferred until the
> §6 design is reviewed and stable.

Sketched scenario set (one per invariant or hazard the receiver
touches):

- **RFC0003.1** — WAL-before-ack `[§3.4]`: a request must not
  receive a 2xx (or gRPC OK) response until every record in the
  batch is durably written to the WAL.
- **RFC0003.2** — Crash-before-ack `[§3.4]`: receiver killed
  between WAL write and response; on restart the records are
  present, the client retries, and de-duplication does not
  produce two copies.
- **RFC0003.3** — Tenant fan-out `[§3.7]`: a single export
  containing two `ResourceLogs` from different sources produces
  two distinct per-tenant streams; no record from Resource A
  appears in tenant B's stream.
- **RFC0003.4** — Tenant resolution failure `[§3.7]`: an export
  whose Resource attributes do not resolve to a configured
  tenant rule is rejected with an error that names the missing
  attribute; no records are accepted.
- **RFC0003.5** — gRPC and HTTP/protobuf transports produce the
  identical in-memory `ExportLogsServiceRequest` for a
  byte-equal payload.
- **RFC0003.6** — HTTP/JSON canonicalisation: a valid OTLP/JSON
  request with whitespace and field-ordering variation produces
  the same canonical body bytes that the same logical record
  produces over gRPC + protobuf.
- **RFC0003.7** — `body.kind = Structured` records carry the
  OTLP-canonical JSON encoding of their `AnyValue` body when
  they reach the miner, regardless of the request transport
  (RFC 0001 §6.1 *Body representation*).
- **RFC0003.8** — `body.kind = String` records reach the miner
  with the unwrapped string as `L_raw` (RFC 0001 §6.2 step 0).
- **RFC0003.9** — `severity_number = 0` (UNSPECIFIED) and
  `scope_name = None` records pass through to the miner without
  rejection or coalescing (RFC 0001 §6.1).
- **RFC0003.10** — `dropped_attributes_count` from the wire is
  preserved verbatim on the record (the receiver does not add
  or recompute).
- **RFC0003.11** — Transport errors (malformed protobuf,
  oversize request, invalid `Content-Type`) produce a
  controlled transport-level error response, not a panic.

## 6. Proposed design

### 6.1 Overall shape

The receiver is a single Rust crate (`ourios-ingester` per the
target layout in `CLAUDE.md` §7) exposing two listeners — gRPC
on its own port, HTTP on its own port — that share a single
business-logic layer. The business-logic layer accepts a
decoded `ExportLogsServiceRequest` and:

1. Iterates `ResourceLogs[]`, deriving `tenant_id` per Resource
   via the operator-configured rule (RFC 0001 §6.1 *Tenant
   derivation*).
2. For each `ResourceLogs`, iterates `ScopeLogs[]` and
   `LogRecord[]`, materialising one `OtlpLogRecord` per record.
   The `OtlpLogRecord` is the in-memory shape RFC 0001 §6.1's
   amended record table mirrors; it carries the inherited
   `Resource` attributes and the `InstrumentationScope` name
   and version as fields, so downstream code never needs to
   walk back up the OTLP hierarchy.
3. For each `LogRecord` whose `body.kind != AnyValue::String`,
   canonicalises the body to OTLP-canonical JSON before the
   `OtlpLogRecord` is materialised — the miner sees only
   canonical bytes, never raw protobuf-decoded `AnyValue` trees.
4. Hands each per-tenant stream to `ourios-miner` (one
   `MinerCluster` per process; the cluster routes internally
   per `tenant_id`).
5. After every record in the batch has been accepted by the
   miner AND written to the WAL with fsync, returns a
   transport-level success.

### 6.2 Wire stack defaults

- gRPC: `tonic` + the `opentelemetry-proto` crate's generated
  `LogsServiceServer` trait.
- HTTP: `axum` on `hyper`. A single `/v1/logs` POST handler
  dispatches on `Content-Type`:
  - `application/x-protobuf` → `prost::Message::decode` into
    the same `ExportLogsServiceRequest` type the gRPC path
    produces.
  - `application/json` → proto3-JSON decode + canonicalisation
    pass.
- Both listeners spawn off the same tokio runtime, share a
  single instance of the business-logic layer, and bind on
  operator-configured ports (defaults TBD, likely 4317 for
  gRPC and 4318 for HTTP per the OTel convention).

### 6.3 Tenant fan-out

Per RFC 0001 §6.1 *Tenant derivation*, `tenant_id` is derived
per `ResourceLogs` group, not per export batch. The receiver:

- Resolves the tenant rule once per `ResourceLogs.resource`.
- Groups the resulting `OtlpLogRecord`s by `tenant_id` (a
  single batch can produce multiple per-tenant groups).
- Hands each group to the miner via `MinerCluster::ingest`,
  which is already per-tenant-routed internally.

If any `ResourceLogs.resource` fails to resolve to a tenant
under the configured rule, the receiver rejects the **entire
batch** with a transport-level error naming the failing
Resource and the missing attribute. Per-Resource partial
acceptance is reserved for a future RFC (see §9).

### 6.4 AnyValue canonicalisation boundary

The receiver — not the miner — performs the AnyValue → OTLP-
canonical JSON conversion for `body.kind != AnyValue::String`
records. Rationale:

- The miner's signature
  (`MinerCluster::ingest(record: &OtlpLogRecord)`) takes a
  ready-to-template record. Pushing the canonicalisation
  upstream keeps the miner's hot path off any
  serialisation/canonicalisation work.
- The two transports (gRPC + protobuf vs HTTP + JSON) need
  different canonicalisation strategies (re-serialise from
  decoded `AnyValue` vs normalise existing JSON). The receiver
  knows which transport delivered the record; the miner
  shouldn't.
- Storing canonicalised JSON in the in-memory `OtlpLogRecord`
  makes the structured-Body short-circuit in RFC 0001 §6.2
  step 0 a constant-time write.

For `body.kind = AnyValue::String`, no canonicalisation is
needed; the unwrapped string is passed through as `L_raw`.

### 6.5 WAL-before-ack sequencing

`[§3.4]` requires the receiver to acknowledge a batch only
after every record is durably written. Concrete contract:

1. Receiver accepts the request and decodes to
   `ExportLogsServiceRequest`.
2. Receiver fans out to per-tenant `OtlpLogRecord` streams
   (§6.3) and canonicalises bodies as needed (§6.4).
3. Receiver appends every record to the WAL.
4. Receiver fsyncs the WAL segment(s) touched.
5. Receiver hands records to the miner for templating.
6. Receiver returns transport-level success.

The fsync-then-template ordering matters: a crash between (4)
and (5) is recoverable (records replay from the WAL; the miner
state is reconstructed); a crash between (3) and (4) loses
those records but the client retries (no ack was sent); a
crash between (5) and (6) is the "the server did the work and
the client never heard about it" case, where client retries
produce duplicates. The dedup mechanism for that retry path is
not yet specified — neither `docs/hazards.md` H3 (WAL durability
vs. latency) nor H8 (replication-induced dedup, currently
forward-looking) covers the single-replica retry case directly.
§9 carries this as an open question.

The receiver itself is post-MVP per `roadmap.md` §5 — the MVP
bench reads OTLP from the on-disk corpus, bypassing this
component entirely. The receiver therefore cannot be enabled
until `ourios-wal` lands, and there is no MVP code path that
acks a network request before durability. The
append-then-fsync-then-ack sequence above is the only
contract; no "WAL no-ops" mode exists, since that would
violate `[§3.4]`.

### 6.6 The `OtlpLogRecord` in-memory shape

The receiver materialises each wire-level `LogRecord` (plus its
inherited `Resource` and `InstrumentationScope` context) into a
single owned struct:

```text
struct OtlpLogRecord {
    // Identity / partitioning
    tenant_id: TenantId,

    // OTLP-derived (per RFC 0001 §6.1)
    time_unix_nano: u64,
    observed_time_unix_nano: Option<u64>,
    severity_number: u8,
    severity_text: Option<String>,
    scope_name: Option<String>,
    scope_version: Option<String>,
    attributes: Vec<KeyValue>,
    dropped_attributes_count: u32,
    resource_attributes: Vec<KeyValue>,
    trace_id: Option<[u8; 16]>,
    span_id: Option<[u8; 8]>,
    flags: u32,
    event_name: Option<String>,

    // Body (already canonicalised per §6.4)
    body_kind: BodyKind,         // String | Structured
    body: BodyPayload,           // String(String) | Structured(Bytes)
}
```

The Rust types are informal here; the precise definition lives
in the `ourios-ingester` crate. The shape mirrors RFC 0001
§6.1 column-for-column so the Parquet writer can serialise a
slice of these directly without a translation layer.

### 6.7 Backpressure (deferred)

The receiver does **not** apply rate limiting in this initial
design. If the miner or the WAL is the bottleneck, the receiver
holds the request open until the per-tenant queue drains, then
acks. In practice this means OTLP clients see backpressure as
elevated request latency rather than as
`partial_success.rejected_log_records`. Whether to upgrade
this to explicit partial-success is reserved for a future RFC
(see §9). The full-failure path (transport error) covers the
unresolvable-tenant and malformed-batch cases per §6.3 and
§3.2.

### 6.8 Out of scope for this RFC

- **Metrics + traces** ingest. OTel Collector and OTLP define
  endpoints for both; Ourios is a logs-only backend per
  `CLAUDE.md` §1. Receiver MAY accept metric/trace requests at
  the transport layer (returning a deliberate `Unimplemented`
  response) but this RFC does not specify that path.
- **mTLS / authn / authz**. Production deployment concerns,
  out-of-band of the OTLP wire contract. A future RFC covers
  the authentication model (likely token-based per request
  with the resolved identity feeding the tenant-derivation
  rule).
- **Schema URL handling**. `ResourceLogs.schema_url` and
  `ScopeLogs.schema_url` are separate OTLP fields and do **not**
  appear on the `OtlpLogRecord` shape in §6.6 — the receiver
  currently drops them. Rationale: RFC 0001 §6.1's record schema
  does not include columns for them, no consumer references them
  yet, and Ourios does not interpret OTel semantic conventions.
  Whether to add `resource_schema_url` / `scope_schema_url`
  fields (or a Parquet column) is tracked as an open question
  in §9; until then the drop is deliberate, not an oversight.
- **Compactor / WAL implementation**. Specified in the
  forthcoming `ourios-wal` RFC; this RFC's contract with the
  WAL is just the append-then-fsync-then-ack sequence in
  §6.5.

## 7. Alternatives considered

### 7.1 Embed `rotel` as a library

`rotel` is a production-quality Rust OTel collector. Embedding
it would give us a known-good OTLP receiver implementation
without us building one. **Rejected** because:

- `rotel`'s public API is collector-shaped (the full
  receivers→processors→exporters pipeline), not "just the
  receiver" shaped. Embedding it means embedding the entire
  pipeline machinery, then building Ourios as one of its
  exporters. That's a deployment shape (out-of-process
  collector) wearing the costume of a code dependency, with
  the worst of both worlds: the dependency footprint of a
  full collector and the integration friction of an in-
  process one.
- The OTel-receiver pieces of `rotel` are themselves built on
  `tonic` + `opentelemetry-proto` — the same primitives we
  would use directly. Embedding `rotel` adds a layer without
  removing one.
- Build-vs-embed parity: our wire-decode layer is small (~a
  few hundred lines, almost all glue against generated
  protobuf bindings). The reuse argument doesn't carry the
  weight it would for a complex piece of infrastructure.

### 7.2 Run the OTel Collector out-of-process and have it forward to us

Common deployment shape: a Collector terminates OTLP at the
network edge, batches, and forwards to a backend. **Rejected
as the default** because:

- The Collector ACKs the OTLP client before our backend sees
  the data, breaking the WAL-before-ack contract `[§3.4]`.
  The only way to recover the contract is for our forwarding
  protocol from the Collector to be itself durable + ack-
  after-fsync — at which point that protocol is what we
  needed to spec, and we are back to writing a receiver.
- Adds a deployment dependency (operator must install and
  configure the Collector) for no signal beyond what a
  direct receiver provides.
- Configuration drift between the Collector's input
  validation and ours becomes a real source of "works in
  one place, fails in the other" bugs.

That said: the Collector is a perfectly fine *deployment
option* for operators who already run one (e.g., for trace
sampling). The receiver in this RFC accepts OTLP from any
source, including a Collector forwarder, so the deployment
shape is not foreclosed; it just isn't the default and doesn't
get to be on the WAL-before-ack path.

### 7.3 Hand-roll the protobuf without `opentelemetry-proto`

Writing our own protobuf bindings against the OTLP `.proto`
files. **Rejected** because the official crate tracks upstream
faithfully and is the canonical Rust binding for the OTLP
messages. Re-implementing risks drift, especially on the
JSON-encoding overrides (hex IDs, base64 bytes) which are
spec-defined but easy to get wrong.

### 7.4 HTTP-only or gRPC-only

Supporting only one of the two transports. **Rejected** because
the OTel ecosystem is split: SDK defaults are gRPC, but HTTP
is widely used in constrained environments and is the standard
exporter target for the Collector's `otlphttp` exporter.
Refusing either transport reduces the receiver to a non-
compliant subset of OTLP and forces a class of operators to
front Ourios with a converter (e.g., the Collector) — which
re-introduces the WAL-before-ack problem of §7.2.

### 7.5 Synchronous AnyValue canonicalisation in the miner instead of the receiver

Pushing the structured-Body canonicalisation step into the
miner rather than the receiver. **Rejected** because:

- The miner's hot path benefits from a constant-time
  body-write in the `body_kind = Structured` short-circuit
  (RFC 0001 §6.2 step 0); doing canonicalisation there moves
  variable-cost serialisation work onto every structured
  record.
- The two transports need different canonicalisation logic
  (re-serialise vs normalise). The miner would need to know
  the source transport, which is a layering inversion.

## 8. Testing strategy

Mapped to the §5 scenarios (sketched at this draft stage; this
mapping is filled when the §5 scenarios are specified):

- **WAL-before-ack** (RFC0003.1, RFC0003.2): an integration
  test running the receiver against a mock WAL that explicitly
  records the order of (append, fsync, ack) events.
  Crash-recovery test SIGKILLs the receiver between fsync and
  ack, restarts, replays.
- **Tenant fan-out** (RFC0003.3, RFC0003.4): hand-curated
  multi-Resource batches, assert that the per-tenant streams
  handed to the miner contain only the records from each
  Resource. Property test on the tenant-derivation rule (any
  rule that returns `Some` for both Resources never produces
  cross-contamination).
- **Wire-decode equivalence** (RFC0003.5, RFC0003.6): a
  property test asserting that a batch of
  `ExportLogsServiceRequest` payloads, serialised to gRPC +
  protobuf, HTTP + protobuf, and HTTP + JSON, round-trips
  through each transport and produces byte-identical canonical
  body bytes for the same logical records.
- **Body canonicalisation** (RFC0003.7, RFC0003.8):
  table-driven tests over the seven `AnyValue` variants, each
  asserting the canonical-JSON output matches the OTLP spec's
  expected encoding. String-Body test asserts the unwrapped
  string is what reaches the miner.
- **Edge OTLP cases** (RFC0003.9, RFC0003.10): hand-curated
  records with `severity_number = 0`, `scope_name = None`,
  non-zero `dropped_attributes_count`. Assert pass-through
  semantics.
- **Transport-level errors** (RFC0003.11): malformed protobuf,
  oversize request, wrong `Content-Type`, etc. Assert
  controlled error responses with no panic.
- **Conformance** (additive): fuzz the receiver with valid
  OTLP batches generated by `proptest` strategies derived
  from the proto definitions.
- **Benchmarks** (`criterion`, in `ourios-bench`):
  end-to-end latency from request arrival to mining-attached,
  for both transports, at typical batch sizes (1, 100, 1000,
  10000 records per batch). Throughput sustained at the WAL
  fsync rate.

`docs/verification.md` §3's two-loop Red gate applies: the §5
scenarios become `#[ignore]`d test stubs at `red` stage, then
get implementations as the receiver crate is built.

## 9. Open questions

- [ ] **Retry-induced duplicate suppression.** A crash between
  miner-attach (step 5) and ack (step 6) in §6.5 produces
  duplicates on client retry. The dedup mechanism (e.g., a
  content-hash idempotency key carried alongside each WAL
  record, or a request-id header from the OTel SDK) is not
  yet specified — the existing hazards (`docs/hazards.md` H3,
  H8) do not cover the single-replica retry case. Settled
  either in the `ourios-wal` RFC or in a follow-up to this
  one.
- [ ] **`ResourceLogs.schema_url` / `ScopeLogs.schema_url`
  preservation.** §6.8 records that schema URLs are currently
  dropped because no consumer references them and RFC 0001
  §6.1's record schema has no column for them. If a
  semantic-conventions-aware feature lands later (e.g., schema
  URL → attribute key mapping), `OtlpLogRecord` and the
  Parquet schema will need the two fields added. Tracked here
  so a future RFC does not re-derive the question.
- [ ] **Where exactly does canonicalisation cost land?**
  Synchronously per record at receive time (simple, predictable
  latency), or batched async (higher throughput, more memory,
  cancellation semantics get harder). §6 commits to
  synchronous; the open question is whether any deployment
  sees this as a bottleneck.
- [ ] **`dropped_attributes_count` semantics on truncation.**
  Preserve verbatim from the wire (current §6 design), sum
  across records, or recompute if the receiver itself drops
  attributes (e.g., for being over the 256-byte limit per RFC
  0001 §3.2)? Current design says preserve; a future receiver-
  side truncation step would need to either recompute or
  use a separate column.
- [ ] **Receiver process model.** Is the receiver a separate
  binary (sidecar shape) or a role of `ourios-server` like
  `ingester`/`querier` (per `CLAUDE.md` §1)? Current
  assumption: a role of the existing `ourios-server` binary,
  toggled by config. Open until the deployment story is
  considered in detail.
- [ ] **Partial-success response semantics.** Is the all-or-
  nothing batch contract (§6.3) sufficient long-term, or do we
  need to expose `partial_success.rejected_log_records` for
  per-tenant-rejection scenarios (e.g., one failing tenant in
  a multi-tenant batch)? Reserved here; deferred to a future
  RFC if a concrete operator need surfaces.
- [ ] **Authentication and tenant binding.** If the receiver
  authenticates the client (mTLS, token), does the
  authenticated identity feed into the `tenant_id` derivation
  rule (e.g., as a constraint), or is it purely an access-
  control check decoupled from tenancy? A future
  authentication RFC settles this; the open question is
  flagged here so the tenant-derivation rule's interface can
  grow into it.
- [ ] **Multi-line / non-UTF-8 body handling for `String`
  bodies.** The miner's tokenize step (RFC 0001 §6.2 step 1)
  has explicit failure modes (malformed UTF-8, embedded NUL,
  oversize). Should the receiver pre-validate and reject at
  the transport level, or pass through and let the miner emit
  a parse-failure record? Current design: pass through,
  per-record granularity is the miner's concern.
- [ ] **Compression (gzip / zstd over HTTP).** OTLP/HTTP
  permits content-encoded request bodies. Is MVP-scope
  receiver expected to support either? Current assumption:
  identity-only at first; gzip at the framework layer if
  `axum`/`hyper` provides it for free; zstd deferred.
- [ ] **Receiver-side OTel telemetry (eating our own dog
  food).** The receiver should itself emit metrics about
  request rates, decode failures, fan-out latency. Specified
  where? Likely in the same RFC as the §6.8 telemetry
  surface (RFC 0001 §6.8); flagged here for tracking.

## 10. References

- OTLP `logs.proto`:
  https://github.com/open-telemetry/opentelemetry-proto/blob/main/opentelemetry/proto/logs/v1/logs.proto
- OTLP `logs_service.proto`:
  https://github.com/open-telemetry/opentelemetry-proto/blob/main/opentelemetry/proto/collector/logs/v1/logs_service.proto
- OTLP `common.proto` (AnyValue, KeyValue):
  https://github.com/open-telemetry/opentelemetry-proto/blob/main/opentelemetry/proto/common/v1/common.proto
- OpenTelemetry Logs Data Model spec:
  https://opentelemetry.io/docs/specs/otel/logs/data-model/
- OTLP transport spec (gRPC, HTTP, encodings):
  https://opentelemetry.io/docs/specs/otlp/
- `tonic`: https://github.com/hyperium/tonic
- `opentelemetry-proto` Rust crate:
  https://crates.io/crates/opentelemetry-proto
- `axum`: https://github.com/tokio-rs/axum
- `rotel`:
  https://github.com/streamfold/rotel
- OpenTelemetry Collector:
  https://github.com/open-telemetry/opentelemetry-collector
- Ourios investigation finding:
  [`docs/architecture/otlp-log-format.md`](../architecture/otlp-log-format.md)
- RFC 0001 §6.1 (record schema this RFC produces records for):
  [`docs/rfcs/0001-template-miner.md`](./0001-template-miner.md)
- `CLAUDE.md` §1 (Ourios is logs-only),
  §3.4 (WAL-before-ack), §3.7 (multi-tenancy not bolted on),
  §4 (hazards).
