---
rfc: 0003
title: OTLP receiver — gRPC and HTTP wire endpoints for OpenTelemetry log ingest
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-05-13
supersedes: —
superseded-by: —
---

# RFC 0003 — OTLP receiver

> **How to read this document.** Sections §§1–4 are the design
> contract — the *what* and the *why*. §5 lists the normative
> `Given / When / Then` scenarios — the contract the receiver
> crate is implemented against and tested for. §6 is the precise
> specification the receiver crate is implemented against. §7
> records the alternatives we evaluated and rejected. §8 maps
> each §5 scenario to the technique that tests it. §9 lists
> open questions; §10 the references.
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
materialises each `LogRecord.body` into the
`Body::String(String) | Body::Structured(AnyValue)` fork (the
decoded `AnyValue` rides through verbatim — canonicalisation
is deferred to the storage layer per the amended §6.4), fans
the batch out into per-tenant streams of `OtlpLogRecord`, hands
each stream to `ourios-miner`, and
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
  carried verbatim into `Body::Structured(AnyValue)` — the
  amended §6.4 defers OTLP-canonical JSON conversion to the
  storage layer (Parquet writer), where the round-trip
  `stored_bytes ↔ AnyValue` per RFC 0001 §6.1 *Body
  representation* makes the `lossy_flag = false` promise
  meetable.
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

Each scenario carries an id of the form `RFC0003.<m>` that is
referenced verbatim from each test's leading doc comment (e.g.
`/// Scenario RFC0003.1 — WAL-before-ack.`) so the spec↔test
mapping is greppable (per `docs/rfcs/README.md` *Required
sections* and `docs/verification.md` §2.3 — function names are
not part of the contract, the doc-comment line is). Scenarios `.1`–`.11` cover the
invariants and hazards the §6 design touches; `.12`–`.15` pin
behaviour the OTLP spec mandates and that the §9 enrichments
surfaced (empty request, compression, default path,
concurrency).

> **Scenario RFC0003.1 — WAL-before-ack `[§3.4]`**
> - **Given** a `Receiver` wired to a real `Wal` (opened with
>   defaults) and a single OTLP `ExportLogsServiceRequest`
>   carrying ≥ 1 `LogRecord`
> - **When** the receiver runs its accept path
> - **Then** the transport-level success response (gRPC `OK` /
>   HTTP 2xx) is emitted only after the `Wal::sync` call
>   covering the batch's frames returns `Ok(_)` — measured by
>   an `AtomicBool` set **after** `sync` returns `Ok(_)`
>   (mirroring RFC0008.1; the probe inside `sync` would
>   already be true mid-call). The response-writer asserts
>   the flag is `true` before sending
> - **And** the pre-sync points (`decode`, `tenant_derive`,
>   `body_materialise`, `append`) all observe the flag as
>   `false`
> - **And** the WAL contains a single `FrameKind::OtlpBatch`
>   frame (per RFC 0008 §4) whose payload bytes equal the
>   encoded `ExportLogsServiceRequest`, before the ack fires
>   — verified by replaying a fresh `Wal::open` after the
>   response and asserting one new frame whose payload
>   round-trips via `prost` to the input request
> - **And** the §6.5 step-5 miner-acceptance precondition for
>   ack also holds: every record in the batch has been
>   handed to `MinerCluster::ingest` and accepted before the
>   ack fires (an instrumented `MinerCluster` stub records
>   each `ingest` call; the response-writer asserts the
>   per-batch accepted-count equals the batch's record-count
>   before sending)

> **Scenario RFC0003.2 — Crash-before-ack: at-least-once with retry tolerance `[§3.4]`**
> - **Given** a receiver wired to a real `Wal`, an OTLP
>   client that retries on transport timeout per the OTLP
>   spec retry semantics, and a `SIGKILL` injected after
>   `Wal::sync` returns `Ok(_)` but before the success
>   response reaches the wire — i.e. anywhere in the §6.5
>   window between step 4 (fsync return) and step 6 (ack);
>   both the step-4/5 and step-5/6 gaps reduce to the same
>   duplicate-on-retry contract since the records are
>   already durable
> - **When** the receiver process restarts, `Wal::replay`
>   runs, and the client retries the timed-out export
> - **Then** the post-restart WAL contains the `OtlpBatch`
>   frame the killed process had fsync'd before the kill —
>   its payload still decodes byte-for-byte to the killed
>   process's input `ExportLogsServiceRequest` (the
>   RFC0008.2 guarantee this RFC consumes)
> - **And** the client's retry is accepted and produces a
>   *second* `OtlpBatch` frame whose payload bytes equal the
>   first — this duplication is the **at-least-once**
>   contract per the OTLP spec's *duplicate-data* section
>   ("duplicate data is a deliberate tradeoff for telemetry
>   data"); the receiver implements no de-duplication in
>   this RFC
> - **And** no special "retry" marker is appended; the receiver
>   has no dedup key (§9 reserves any future dedup mechanism
>   for a follow-up RFC) and cannot distinguish a retry from
>   an independent batch carrying the same records

> **Scenario RFC0003.3 — Tenant fan-out `[§3.7]`**
> - **Given** an OTLP batch containing exactly two
>   `ResourceLogs` groups R_A (`service.name = "svc-a"`) and
>   R_B (`service.name = "svc-b"`), and an operator-configured
>   tenant-derivation rule keyed on `service.name`
> - **When** the receiver processes the batch
> - **Then** the `(tenant_id, OtlpLogRecord)` pairs accepted
>   by an instrumented `MinerCluster` stub for `tenant_id_a`
>   are exactly those derived from R_A and contain no record
>   derived from R_B
> - **And** the symmetric assertion holds for `tenant_id_b`
> - **And** each emitted `OtlpLogRecord`'s `resource_attributes`
>   reflects the originating Resource verbatim — the receiver
>   does not mix Resource attribute sets across the fan-out

> **Scenario RFC0003.4 — Tenant resolution failure rejects the entire batch `[§3.7]`**
> - **Given** an OTLP batch where at least one
>   `ResourceLogs.resource` lacks the attribute named by the
>   operator's tenant-derivation rule
> - **When** the receiver processes the batch
> - **Then** the receiver emits a transport-level error
>   (gRPC `INVALID_ARGUMENT` / HTTP 400) whose payload names
>   the failing `ResourceLogs` index and the missing
>   attribute key
> - **And** no record from the batch is appended to the WAL
>   (asserted by reopening the WAL post-test and observing
>   that frame count and segment offsets are unchanged from
>   the pre-batch snapshot)
> - **And** no record from the batch reaches
>   `MinerCluster::ingest` — per-Resource partial acceptance
>   is reserved per §6.3

> **Scenario RFC0003.5 — gRPC ≡ HTTP/protobuf decode equivalence**
> - **Given** a byte-equal `ExportLogsServiceRequest`
>   protobuf payload
> - **When** the payload is decoded via the `tonic` gRPC
>   handler and via the `axum` HTTP handler with
>   `Content-Type: application/x-protobuf` independently
> - **Then** the two resulting in-memory
>   `ExportLogsServiceRequest` values are structurally equal
>   (`PartialEq`), and every derived `OtlpLogRecord` from
>   each path is field-for-field equal — including `body`,
>   `attributes`, `resource_attributes`, `trace_id`,
>   `span_id`, and `dropped_attributes_count`

> **Scenario RFC0003.6 — HTTP/JSON ↔ gRPC/protobuf equivalence with OTLP-JSON encoding rules**
> - **Given** an `ExportLogsServiceRequest` carrying
>   non-trivial `trace_id` and `span_id` bytes, a record with
>   a `bytes`-typed `AnyValue` attribute, and at least one
>   record whose `severity_number` exercises a non-default
>   enum value
> - **When** the payload is serialised as gRPC + protobuf and
>   as HTTP + `application/json` per the OTLP-JSON mapping
>   (hex-encoded `traceId` / `spanId`, base64-encoded
>   `bytes`, integer-encoded enums, lowerCamelCase field
>   names) and each is independently decoded by the receiver
> - **Then** the two derived `OtlpLogRecord` sequences are
>   equal at the `AnyValue` tree level — no byte-level
>   canonicalisation is asserted at this layer (canonical-JSON
>   equivalence is the Parquet writer's contract per §6.4)
> - **And** the JSON decoder accepts whitespace and field-
>   ordering variation (insignificant per proto3-JSON)
> - **And** the JSON decoder ignores unknown top-level fields
>   in the request body per the OTLP spec's "receivers MUST
>   ignore unknown fields" rule (forward-compatibility)

> **Scenario RFC0003.7 — `Body::Structured` carries the decoded `AnyValue` verbatim**
> - **Given** a `LogRecord` whose `body` is an `AnyValue` of
>   a non-`string_value` variant (`kvlist_value`,
>   `array_value`, `int_value`, `double_value`, `bool_value`,
>   or `bytes_value`)
> - **When** the receiver materialises the record via
>   `ourios_core::otlp::Body::from_any_value` from either
>   transport
> - **Then** the resulting `OtlpLogRecord.body` is
>   `Some(Body::Structured(av))` where `av` is structurally
>   equal to the wire's `AnyValue` (no canonicalisation, no
>   reshape, no dropped fields)
> - **And** the same equality holds across the three
>   transports, since RFC0003.5 and RFC0003.6 make the
>   per-transport decodes equivalent at the `AnyValue` level

> **Scenario RFC0003.8 — `Body::String` reaches the miner as the unwrapped `L_raw`**
> - **Given** a `LogRecord` whose `body` is
>   `AnyValue { value: Some(string_value(s)) }`
> - **When** the receiver materialises the record
> - **Then** the resulting `OtlpLogRecord.body` is
>   `Some(Body::String(s))` where `s` is the original UTF-8
>   string (no wrapping, no quoting, no escaping)
> - **And** the value handed to `MinerCluster::ingest`
>   matches `s` exactly when read back via the miner's
>   RFC 0001 §6.2 step-0 short-circuit path

> **Scenario RFC0003.9 — Edge OTLP fields pass through unchanged**
> - **Given** a `LogRecord` with `severity_number = 0`
>   (`UNSPECIFIED`), no `scope_name` on its enclosing
>   `InstrumentationScope`, and `observed_time_unix_nano = 0`
> - **When** the receiver materialises the record
> - **Then** the derived `OtlpLogRecord` carries
>   `severity_number = 0`, `scope_name = None`, and
>   `observed_time_unix_nano = None` (per RFC 0001 §6.1's
>   optionality)
> - **And** the record is accepted by `MinerCluster::ingest`
>   without rejection, coalescing, substitution, or any
>   downcast to a "default" value

> **Scenario RFC0003.10 — `dropped_attributes_count` preserved verbatim**
> - **Given** a `LogRecord` whose `dropped_attributes_count`
>   is `42` on the wire
> - **When** the receiver materialises the record
> - **Then** the resulting `OtlpLogRecord.dropped_attributes_count`
>   is exactly `42`
> - **And** the receiver does not recompute the field — it
>   reflects the *wire-level* claim only, even if a future
>   receiver-side per-attribute truncation step would have
>   dropped further attributes (a hypothetical such step is
>   tracked as a §9 open question)

> **Scenario RFC0003.11 — Transport-level errors are controlled, not panics**
> - **Given** any of:
>   - a malformed protobuf payload (random bytes that fail
>     `prost::Message::decode`),
>   - an over-size request body exceeding the receiver's
>     configured request-size limit,
>   - an HTTP request with an unrecognised `Content-Type`,
>   - an HTTP `POST` to a path other than the configured
>     `/v1/logs` (covered jointly with RFC0003.14),
>   - or a gRPC client cancellation mid-decode
> - **When** the receiver handles the request
> - **Then** the receiver emits a controlled transport-level
>   error — gRPC `INVALID_ARGUMENT` / `RESOURCE_EXHAUSTED` /
>   `CANCELLED` as appropriate, or HTTP 400 / 413 / 415 / 404
>   as appropriate
> - **And** no part of the receiver panics or restarts; the
>   process remains alive (each arm of the test asserts this
>   after the request)
> - **And** no partial record is appended to the WAL

> **Scenario RFC0003.12 — Empty `ExportLogsServiceRequest` returns success without WAL write**
> - **Given** an `ExportLogsServiceRequest` with
>   `resource_logs` empty
> - **When** the receiver processes the request via either
>   transport
> - **Then** the receiver emits a transport-level success
>   response carrying an `ExportLogsServiceResponse` with
>   `partial_success` unset (per the OTLP spec's
>   *otlpgrpc-response* and *otlphttp-response* sections:
>   "servers SHOULD treat empty as success")
> - **And** the receiver does not invoke `Wal::sync`, no
>   frame is appended (asserted via a test wrapper around the
>   `Wal` handle that counts `append` and `sync` calls), and
>   no record reaches `MinerCluster::ingest`

> **Scenario RFC0003.13 — Compression: identity and gzip MUST be supported**
> - **Given** an HTTP request whose body is the byte-equal
>   `ExportLogsServiceRequest` payload of RFC0003.5,
>   transported with `Content-Encoding: identity` (or absent)
>   and with `Content-Encoding: gzip` independently
> - **When** the receiver processes each request
> - **Then** the two derived `OtlpLogRecord` sequences are
>   equal — the OTLP spec mandates both encodings, and the
>   receiver's decode produces semantically identical results
> - **And** a request with an unsupported `Content-Encoding`
>   (e.g. `zstd`, `br`) is rejected with HTTP 415 and a
>   controlled error message; `zstd` support is deferred per
>   §9

> **Scenario RFC0003.14 — Default `/v1/logs` path with configurable override**
> - **Given** the HTTP listener bound with the default path
>   configuration
> - **When** a `POST` arrives at `/v1/logs`
> - **Then** the receiver handles it via the OTLP/HTTP code
>   path defined in §6.2
> - **And** a `POST` to any other path returns HTTP 404 (the
>   "wrong path" arm of RFC0003.11)
> - **And** when the operator configures an override path
>   (e.g. `/otlp/v1/logs`), it replaces `/v1/logs` as the
>   accepted path without changing any other receiver
>   behaviour (the configurability matches the Collector's
>   OTLP-receiver `path` knob, so deployments that need a
>   non-standard prefix don't have to front Ourios with a
>   reverse proxy)

> **Scenario RFC0003.15 — Concurrent `Export` calls each obey WAL-before-ack independently `[§3.4]`**
> - **Given** N ≥ 2 concurrent gRPC `Export` unary calls
>   submitted to the receiver from independent client
>   connections
> - **When** each call's batch independently traverses the
>   §6.5 sequence
> - **Then** each call's ack is emitted only after its own
>   batch's `Wal::sync` returns `Ok(_)` *and* its own batch's
>   records have all been accepted by `MinerCluster::ingest`
>   — the §3.4 `AtomicBool` of RFC0003.1 is per in-flight
>   call, not process-global; a per-call probe records both
>   the sync-completion and miner-acceptance ordering before
>   the response-writer sends
> - **And** the WAL contains exactly one `OtlpBatch` frame
>   per concurrent call (no call's batch is lost to
>   concurrency, asserted by replay producing N frames whose
>   payloads round-trip to the N input
>   `ExportLogsServiceRequest`s)
> - **And** the test does *not* assert any cross-call
>   ordering — concurrent batches may interleave in the WAL
>   as the tokio runtime chooses, which is consistent with
>   the OTLP spec's recommendation to support concurrent
>   unary `Export` calls for throughput

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
3. For each `LogRecord`, materialises `body` into the
   `Body::String(String) | Body::Structured(AnyValue)` fork
   per `ourios-core::otlp::Body::from_any_value`. No
   canonicalisation runs at the receiver — the structured
   branch carries the decoded `AnyValue` verbatim per the
   amended §6.4.
4. Hands each per-tenant stream to `ourios-miner` (one
   `MinerCluster` per process; the cluster routes internally
   per `tenant_id`).
5. After the batch has been written to the WAL as a single
   `OtlpBatch` frame with fsync AND every record accepted by
   the miner, returns a transport-level success.

### 6.2 Wire stack defaults

- gRPC: `tonic` + the `opentelemetry-proto` crate's generated
  `LogsServiceServer` trait.
- HTTP: `axum` on `hyper`. A single `/v1/logs` POST handler
  dispatches on `Content-Type`:
  - `application/x-protobuf` → `prost::Message::decode` into
    the same `ExportLogsServiceRequest` type the gRPC path
    produces.
  - `application/json` → proto3-JSON decode into
    `ExportLogsServiceRequest`. The decode handles whitespace
    and field-ordering variation natively (proto3-JSON spec);
    no separate canonicalisation pass — the
    `Body::Structured(AnyValue)` carried downstream is
    transport-agnostic at the `AnyValue` level.
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

### 6.4 AnyValue canonicalisation is deferred to the storage layer

> **Amendment (PR introducing `ourios-core::otlp::OtlpLogRecord`).**
> This subsection originally pinned the receiver as the place
> that canonicalises structured `AnyValue` bodies into
> OTLP-canonical JSON, with the in-memory record carrying
> pre-cached `Bytes`. The amended position carries the
> `AnyValue` *itself* on the in-memory record and defers
> canonicalisation to the storage layer (Parquet writer,
> when it lands). Rationale below.

The receiver hands the miner an `OtlpLogRecord` whose body, when
present and structured, carries the decoded `AnyValue` verbatim
(`Body::Structured(AnyValue)`). The miner runs the §6.2 step-0
short-circuit on the discriminator alone; it does not walk the
`AnyValue` tree. Canonicalisation to OTLP-canonical JSON
(per RFC 0001 §6.1 *Body representation*) happens once, at
Parquet-write time.

Rationale:

- **Optionality.** RFC 0001 §6.1 reserves a future "mine inner
  field" mode (e.g. mine `body.kvlist["msg"]` as the line if
  present) gated on corpus evidence. That mode needs the
  structured tree, not pre-cached bytes. Carrying `AnyValue`
  preserves the option without committing to the design.
- **Single canonicalisation pass.** Whether the body arrived
  as gRPC-protobuf or HTTP-JSON, the storage layer performs
  exactly one canonicalisation step at write time. The
  receiver no longer has to know two canonicalisation
  strategies (re-serialise vs normalise); the storage layer
  owns the one canonical mapping per RFC 0001 §6.1.
- **Miner hot path is unchanged.** The §6.2 step-0
  short-circuit only inspects the discriminator
  (`Body::Structured(_)` vs `Body::String(_)`); no
  `AnyValue` walking, no allocation in the structured branch.
  The hot-path argument that originally favoured pre-caching
  was speculative — until corpus benchmarks say otherwise,
  carrying the tree costs nothing the miner cares about.

For `Body::String(s)`, no canonicalisation is ever needed; the
unwrapped string is passed through as `L_raw`.

### 6.5 WAL-before-ack sequencing

`[§3.4]` requires the receiver to acknowledge a non-empty
batch only after the batch's `OtlpBatch` frame is durably
written. (The empty-batch fast path of RFC0003.12 is the
explicit exception: no WAL write occurs, and success is
returned without an `OtlpBatch` frame.) Concrete contract for
the non-empty case:

1. Receiver accepts the request and decodes to
   `ExportLogsServiceRequest`.
2. Receiver fans out to per-tenant `OtlpLogRecord` streams
   (§6.3); body canonicalisation does not happen here, per the
   amended §6.4.
3. Receiver appends the encoded request as a single
   `FrameKind::OtlpBatch` frame (verbatim
   `ExportLogsServiceRequest` protobuf bytes, per RFC 0008
   §4) to the WAL.
4. Receiver fsyncs the WAL segment(s) touched.
5. Receiver hands records to the miner for templating.
6. Receiver returns transport-level success.

The fsync-then-template ordering matters: a crash between (4)
and (5) is recoverable (records replay from the WAL; the miner
state is reconstructed); a crash between (3) and (4) loses
those records but the client retries (no ack was sent); a
crash between (5) and (6) is the "the server did the work and
the client never heard about it" case, where client retries
produce duplicates. This RFC implements no de-duplication:
duplicates on retry are the explicit at-least-once contract
per RFC0003.2 and §9 #1 (resolved by reference to the OTLP
spec's *duplicate-data* section). Any future content-hash or
request-id dedup is purely additive on top of this baseline.

The receiver itself is post-MVP per `roadmap.md` §5 — the MVP
bench reads OTLP from the on-disk corpus, bypassing this
component entirely. The receiver therefore cannot be enabled
until `ourios-wal` lands, and there is no MVP code path that
acks a network request before durability. The
append-then-fsync-then-ack sequence above is the only
contract; no "WAL no-ops" mode exists, since that would
violate `[§3.4]`.

### 6.6 The `OtlpLogRecord` in-memory shape

> **Amendment (PR introducing `ourios-core::otlp::OtlpLogRecord`).**
> Body now carries the decoded `AnyValue` rather than its
> OTLP-canonical JSON encoding (see amended §6.4).
> `body_kind` is derived from `body` rather than stored on
> the record, since the §6.2 step-0 fork only needs to read
> the discriminator.

The receiver materialises each wire-level `LogRecord` (plus its
inherited `Resource` and `InstrumentationScope` context) into a
single owned struct. The authoritative definition lives in the
`ourios-core::otlp` module; the sketch below mirrors that
module:

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
    attributes: Vec<KeyValue>,            // opentelemetry-proto KeyValue
    dropped_attributes_count: u32,
    resource_attributes: Vec<KeyValue>,   // opentelemetry-proto KeyValue
    trace_id: Option<[u8; 16]>,
    span_id: Option<[u8; 8]>,
    flags: u32,
    event_name: Option<String>,

    // Body — None when the wire delivered no body
    body: Option<Body>,
}

enum Body {
    String(String),
    Structured(AnyValue),                 // opentelemetry-proto AnyValue
}

// `body_kind()` is a method on OtlpLogRecord that returns
// `Option<BodyKind>` derived from `body`; the discriminator
// is never stored.
enum BodyKind { String, Structured }
```

The Rust types are informal here; the precise definition lives
in the `ourios-core::otlp` module — owning the type in
`ourios-core` (rather than `ourios-ingester`) lets the miner
take it without depending on the receiver crate, since the
receiver doesn't yet exist. The shape mirrors RFC 0001 §6.1
column-for-column so the Parquet writer can serialise a slice
of these directly without a translation layer.

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

### 7.5 Synchronous AnyValue canonicalisation in the miner or the receiver

Two related alternatives evaluated together:

**(a) Canonicalise in the miner.** **Rejected** because the
miner's hot path benefits from a constant-time write in the
`body_kind = Structured` short-circuit (RFC 0001 §6.2 step 0);
doing serialisation work there scales with body size on every
structured record. The miner would also need to know the
source transport (the two transports need different
canonicalisation strategies), which is a layering inversion.

**(b) Canonicalise in the receiver before materialising
`OtlpLogRecord`.** This was the original §6.4 stance and was
the basis on which §7.5(a) was rejected. **Reversed** by the
§6.4 amendment: receiver-side canonicalisation forecloses
the future "mine inner field" mode (RFC 0001 §6.1) which
needs the structured tree, not pre-cached bytes; it also
splits canonicalisation knowledge across two transports
unnecessarily. The current commitment is **canonicalise at
the storage layer** (Parquet writer), where the OTel
JSON-encoding overrides (hex IDs, base64 bytes) live in one
place and run once per record at write time.

The miner-as-canonicaliser variant (a) remains rejected on
its original grounds.

## 8. Testing strategy

Mapped to the §5 scenarios. Each technique below names the
scenario ids it covers; each test's leading doc comment
references the same id verbatim
(`/// Scenario RFC0003.1 — WAL-before-ack.` etc., per
`docs/verification.md` §2.3) so the spec↔test mapping is
greppable.

- **WAL-before-ack and concurrency** (RFC0003.1, RFC0003.15):
  integration tests against a real `Wal` (defaults), with an
  `AtomicBool` ordering probe mirroring RFC0008.1 — set after
  `Wal::sync` returns, asserted `true` by the response-writer
  and `false` by every pre-sync stage. RFC0003.15 spawns N ≥ 2
  concurrent `Export` calls and uses a *per-call* probe so the
  invariant is checked independently per in-flight call.
- **Crash-before-ack** (RFC0003.2): a child-process harness
  mirroring `wal_crash_fixture` (PR #126) runs a receiver
  binary wired to a real `Wal`, the parent SIGKILLs between
  `Wal::sync` return and ack-emit, the parent restarts the
  child and re-issues the export, and the assertion is that
  the post-restart WAL contains the same `OtlpBatch` payload
  bytes *twice* (two frames, one per export attempt) — the
  at-least-once contract per the OTLP spec's *duplicate-data*
  section. The test explicitly does *not* assert dedup;
  RFC0003.2's contract is "no loss + safe retry," not
  exactly-once.
- **Tenant fan-out** (RFC0003.3, RFC0003.4): unit tests with a
  hand-curated two-Resource batch and an instrumented
  `MinerCluster` stub that records every accepted
  `(tenant_id, OtlpLogRecord)` pair. A `proptest` strategy
  over tenant-derivation rules asserts the
  cross-contamination-free invariant for any rule that returns
  `Some` for both Resources. RFC0003.4 uses a hand-curated
  batch where one Resource lacks the rule's attribute key.
- **Wire-decode equivalence** (RFC0003.5, RFC0003.6): a
  `proptest` strategy generates `ExportLogsServiceRequest`
  payloads across the proto's value space; each is serialised
  to gRPC + protobuf, HTTP + protobuf, and HTTP + JSON,
  decoded by the receiver, and the three resulting
  `OtlpLogRecord` sequences are asserted equal at the
  `AnyValue` level. The RFC0003.6 OTLP-JSON encoding-rule
  clauses (hex IDs, base64 bytes, integer enums, ignore
  unknown fields) use hand-curated payloads, since the
  proptest generator can't reliably exercise spec-mandated
  forward-compatibility behaviour.
- **Body fork** (RFC0003.7, RFC0003.8): table-driven tests
  over all seven `AnyValue` variants assert
  `Body::from_any_value` routes `string_value` to
  `Body::String(s)` (unwrapped) and every other variant to
  `Body::Structured(av)` with `av` structurally equal to the
  input and the inner `oneof` moved, not cloned.
- **Edge OTLP cases** (RFC0003.9, RFC0003.10): hand-curated
  `LogRecord`s exercising `severity_number = 0`,
  `scope_name = None`, `observed_time_unix_nano = 0`, and
  non-zero `dropped_attributes_count`. Assertions pin the
  pass-through semantics on the derived `OtlpLogRecord`.
- **Transport-level errors + empty request** (RFC0003.11,
  RFC0003.12): table-driven tests over each error arm
  (malformed protobuf, oversize, unrecognised `Content-Type`,
  wrong path, mid-decode cancellation) and the empty-request
  success arm. Each assertion pins the response status code,
  that no record reaches the WAL or miner, and that the
  receiver process is still alive afterwards.
- **Compression and path** (RFC0003.13, RFC0003.14): the gzip
  arm of RFC0003.13 uses `flate2` to construct the
  `Content-Encoding: gzip` body; the unsupported-encoding arm
  asserts HTTP 415. RFC0003.14's path arm covers the default
  `/v1/logs`, a wrong-path 404, and an operator-configured
  override path producing equivalent behaviour.
- **Conformance fuzzing** (additive, not bound to a single
  scenario): `proptest` strategies derived from the proto
  definitions feed random valid batches through the receiver;
  the only assertion is "no panic; response is either success
  or a controlled transport-level error" — a backstop against
  decode paths the hand-curated cases miss.
- **Benchmarks** (`criterion`, in `ourios-bench`): end-to-end
  latency from request arrival to ack-fires, for both
  transports, at batch sizes (1, 100, 1 000, 10 000 records
  per batch). RFC0003.15 throughput at N = 8 concurrent
  callers. Regressions block merges per `CLAUDE.md` §6.2.

`docs/verification.md` §3's two-loop Red gate applies: the §5
scenarios become `#[ignore]`d test stubs at `red` stage, then
get implementations as the receiver crate is built (mirroring
the PR-M2 pattern that landed RFC0008 §5).

## 9. Open questions

- [x] ~~**Retry-induced duplicate suppression.**~~
  *Resolved by §5 / RFC0003.2:* a crash between miner-attach
  (step 5) and ack (step 6) in §6.5 produces duplicates on
  client retry, and that is the contract. The OTLP spec's
  *duplicate-data* section ("the client may re-send … which
  may result in duplicate data on the server side. This is a
  deliberate choice and is considered to be the right
  tradeoff for telemetry data") explicitly accepts
  at-least-once with duplicates; the Collector's WAL
  guidance carries the same caveat. The receiver implements
  no de-duplication in this RFC. If a future RFC introduces
  a dedup mechanism (content-hash idempotency key, OTel SDK
  request-id header), it is purely additive — the
  at-least-once baseline is the floor, not a stop-gap.
- [ ] **`ResourceLogs.schema_url` / `ScopeLogs.schema_url`
  preservation.** §6.8 records that schema URLs are currently
  dropped because no consumer references them and RFC 0001
  §6.1's record schema has no column for them. If a
  semantic-conventions-aware feature lands later (e.g., schema
  URL → attribute key mapping), `OtlpLogRecord` and the
  Parquet schema will need the two fields added. Tracked here
  so a future RFC does not re-derive the question.
- [x] ~~**Where exactly does canonicalisation cost land?**~~
  *Resolved by the §6.4 amendment:* canonicalisation runs at
  the storage layer (Parquet writer) at write time. The
  receiver carries the decoded `AnyValue` verbatim; the miner
  doesn't canonicalise. Whether the storage layer batches
  canonicalisation or runs it per record is a Parquet-writer
  RFC concern, not a receiver concern.
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
- [x] ~~**Compression (gzip / zstd over HTTP).**~~
  *Resolved by §5 / RFC0003.13:* the OTLP spec mandates that
  servers support `identity` and `gzip`; both are required
  acceptance criteria. `zstd` and `br` are out of scope for
  this RFC — a request carrying an unsupported encoding is
  rejected with HTTP 415. A future RFC may add `zstd` if
  operator demand surfaces; until then the 415 response is
  the contract.
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
