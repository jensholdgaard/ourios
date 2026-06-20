---
rfc: 0018
title: OTLP log-spec compliance amendments
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-20
supersedes: —
superseded-by: —
---

# RFC 0018 — OTLP log-spec compliance amendments

## 1. Summary

Close the OpenTelemetry OTLP log-spec gaps surfaced by the 2026-06-20
compliance audit, as one push. Ourios is an OTLP-native log backend, so
**spec fidelity outranks downstream API stability** — these amendments
break and extend public types where the spec requires it. Six fixes
spanning three `green` RFCs (0002, 0003, 0005): (1) persist the dropped
`InstrumentationScope.attributes` and the per-resource / per-scope
`schema_url` (a flat OTLP **MUST** — `AnyValue` and the scope tuple must be
preserved); (2) map transient ingest failures to **retryable** gRPC/HTTP
codes instead of non-retryable `INTERNAL`/`500` (clients currently *drop*
data they should retry); (3) make `event_name` a first-class DSL filter;
(4) round-trip non-finite doubles in canonical `AnyValue` JSON; (5)
**preserve** out-of-range `SeverityNumber` + flag it instead of the current
silent clamp-to-`0` (the backend is a faithful witness, not a corrector —
§3.0); (6) correct the `body` column documentation.
This amends RFC 0002 (DSL), RFC 0003 (receiver), and RFC 0005 (schema).

## 2. Motivation

A three-area audit (receiver, schema, DSL/querier), graded against the
OTLP/OTel spec's own MUST/SHOULD levels, found that the ingest and query
paths under-serve the spec in ways a fidelity-first backend must not. The
gaps were verified against the OpenTelemetry knowledge base; one audit
claim (that structured `AnyValue` bodies are "type-erased") was **refuted**
— the canonical JSON Ourios stores *is* the OTLP protobuf→JSON mapping and
preserves the `AnyValue` discriminator, so it is not in scope here.

The spec is unambiguous on the load-bearing gap: `AnyValue`s expressing
empty/zero/empty-string/empty-array "are considered meaningful and **MUST**
be stored and passed on to processors / exporters"
([common/#anyvalue](https://opentelemetry.io/docs/specs/otel/common/#anyvalue)),
and the instrumentation scope is the `(name, version, schema_url,
attributes)` tuple
([common/instrumentation-scope](https://opentelemetry.io/docs/specs/otel/common/instrumentation-scope/)).
Ourios decodes only `name`/`version` and discards the rest at the receiver
boundary, so those fields never reach Parquet and RFC 0017's `LogRow` can
never return them. The retry-semantics gap is a quieter data-loss bug:
mapping a transient WAL/storage failure to gRPC `INTERNAL` (which the OTLP
retry table marks **non-retryable**) tells the client to *drop* the batch
([otlp/#failures](https://opentelemetry.io/docs/specs/otlp/#failures)).

"One compliance push" was the maintainer's chosen sequencing: clear the
whole list before more read-path work, so the query read path — RFC 0016's
serving endpoint returning RFC 0017's `LogRow` — lands on a complete,
spec-faithful schema.

## 3. Proposed design

### 3.0 Governing principle — faithful witness, not corrector

Producing spec-valid telemetry is the **upstream's** contract — the SDK, the
instrumenting library, and any intermediary collectors/processors (OTel
ships the tooling for it: severity parsers, the transform processor). When
an upstream emits non-compliant data (e.g. `SeverityNumber = 25`), *it*
broke the contract. Ourios is the storage/query backend, not the
normalizer, so its job is to be a **faithful witness**:

1. **Preserve** what arrived, byte-for-byte, up to the point where a
   storage invariant physically forbids it.
2. **Surface** any spec violation as an observable anomaly (a metric, and
   where practical a marker on read) — so an operator can *find* the
   misbehaving upstream.
3. **Never silently correct** (clamping/normalizing destroys the evidence
   and masks the upstream bug) **nor silently reject** (dropping punishes
   the operator for an upstream they may not control, and loses logs).

This is Postel's law, and it matches what OTLP already asks of receivers
elsewhere — tolerate unknown fields, preserve unknown (open-)enum values.
The spec gives backends latitude in *representation* ("Backend and UI **may**
represent..."), never a mandate to *correct*; "normalized to values
described" is a producer-side mapping rule, not a backend one. Normalization
on ingest, if ever wanted, is a future **opt-in** config (gated on a
concrete consumer), never the silent default. This principle governs the
gaps below — most visibly §3.5.

### 3.1 Persist `InstrumentationScope.attributes` + `schema_url` (RFC 0003 + RFC 0005) — the MUST

The receiver (`crates/ourios-ingester/src/receiver/materialize.rs`) decodes
`scope.name` and `scope.version` but drops `scope.attributes`,
`ResourceLogs.schema_url`, and `ScopeLogs.schema_url`. Add to the decode
and to `OtlpLogRecord` / `MinedRecord` (`crates/ourios-core/src/otlp.rs`,
`record.rs`):

- `scope_attributes` — the scope's `KeyValue` list, encoded as canonical
  JSON exactly like `attributes` / `resource_attributes` (RFC 0005 §3.3),
  empty → `[]`;
- `resource_schema_url` — the `ResourceLogs.schema_url` string;
- `scope_schema_url` — the `ScopeLogs.schema_url` string.

Add three OPTIONAL columns to the RFC 0005 §3.2 schema
(`crates/ourios-parquet/src/lib.rs`):

| column | type | required | note |
|---|---|---|---|
| `scope_attributes` | STRING (canonical JSON) | OPTIONAL | per RFC 0005 §3.3 encoding; `[]` when empty, NULL only in pre-amendment files |
| `resource_schema_url` | STRING | OPTIONAL | OTLP `ResourceLogs.schema_url` |
| `scope_schema_url` | STRING | OPTIONAL | OTLP `ScopeLogs.schema_url` |

All three are **OPTIONAL** for the RFC 0005 §3.5 migration rule alone
(additive columns; readers MUST tolerate their absence in historical
files) — **not** as a value encoding. The two `schema_url` columns
distinguish present-but-empty from absent: a wire `schema_url = ""` is
stored as `""` (a present empty value), and `NULL` is reserved for the
historical "column missing" case; `scope_attributes` follows the
`attributes` convention (`[]` when empty, `NULL` only pre-amendment).
`scope_attributes` rides the §3.3 canonical encoder/decoder unchanged, so
it inherits its round-trip property tests. This is the only
gap the spec makes a flat **MUST**; it is also the prerequisite for RFC
0017's `LogRow` to carry the complete scope and for RFC 0010 drift to see
scope-level `schema_url` changes.

### 3.2 Retryable error mapping for transient failures (RFC 0003)

`crates/ourios-ingester/src/receiver/grpc.rs` maps all non-tenant-resolution
failures (including WAL append / fsync failures) to `Status::internal`, and
`http.rs` maps them to `500`. Per the OTLP retry table
([otlp/#failures](https://opentelemetry.io/docs/specs/otlp/#failures)),
`INTERNAL` and `500` are **non-retryable** — so a client that hits a
*transient* WAL/storage failure drops the batch instead of retrying,
violating the spirit of WAL-before-ack durability.

Amend the RFC 0003 error-mapping contract to distinguish *transient* from
*permanent*:

- **Transient** (WAL append I/O failure, post-rotation quiesce, fsync
  failure, storage unavailable, ingest saturation) → gRPC `UNAVAILABLE`
  (optionally `RESOURCE_EXHAUSTED` **with** a `RetryInfo` detail for
  saturation, per
  [otlp/#otlpgrpc-throttling](https://opentelemetry.io/docs/specs/otlp/#otlpgrpc-throttling));
  HTTP `503` (optionally `429` for saturation) with an optional
  `Retry-After` header.
- **Permanent** failures stay non-retryable, but are not a single HTTP code:
  malformed payload and tenant-resolution failure → HTTP `400` (gRPC
  `INVALID_ARGUMENT`), while an **oversize payload** (`AppendError::TooLarge`,
  a batch over the 16 MiB WAL frame ceiling) → HTTP `413` (gRPC
  `INVALID_ARGUMENT`). An oversize batch is a client sizing error, *not* a
  WAL outage: retrying it byte-identical can never succeed, so it MUST stay
  non-retryable even though it surfaces as a `WalAppend` error.

The 429/503 *throttling* surface itself remains a **SHOULD** and may stay
minimal (no rate-limiter yet, RFC 0003 §6.7); the binding change here is
that a transient failure MUST NOT be reported with a non-retryable code.

### 3.3 `event_name` as a first-class DSL filter (RFC 0002)

`event_name` is stored (RFC 0005 §3.2) and will be returned by RFC 0017,
but the DSL cannot filter on it. Add an `EventName` variant to the DSL
`Field` enum (`crates/ourios-querier/src/dsl/ir.rs`), a grammar token
`event_name`, and a compile case projecting to the `event_name` column —
mirroring the existing `scope` bare field exactly (RFC 0002 §6.1). String
operators only (`=`, `contains`, …), consistent with other string fields.
Also add `scope_version` as a bare field by the same pattern (currently
only `scope` name is filterable); `scope_attributes` becomes filterable via
the existing `scope.<key>` attribute-path mechanism once §3.1 stores it.

### 3.4 Round-trip non-finite doubles in canonical `AnyValue` JSON (RFC 0005)

The canonical encoder (`crates/ourios-core/src/otlp.rs`) serialises a
non-finite `double_value` (`NaN`, `±Infinity`) to JSON `null`, which does
not decode back to the original — a lossy round-trip pinned by an existing
test. Ourios's canonical encoding is the **OTLP protobuf→JSON mapping**
(proto3 JSON; the same encoding `body`/`attributes` already use, RFC 0005
§3.3), and proto3 JSON represents non-finite floats as the **quoted string
forms** `"NaN"`, `"Infinity"`, `"-Infinity"`. Adopt those string forms
(not the bare `NaN`/`Infinity` tokens — they are invalid JSON and belong to
OTel's separate lossy *non-OTLP-protocol* string encoding, not the
protobuf-JSON mapping), and replace the "encodes to null" test with a
round-trip assertion.

### 3.5 Preserve out-of-range `SeverityNumber`, don't clamp it (RFC 0003)

The receiver **already** clamps: `severity_to_u8`
(`crates/ourios-ingester/src/receiver/materialize.rs:105`) maps any value
outside `0..=24` to `0` (UNSPECIFIED). Per §3.0 this is the wrong default —
it is a *silent correction* that both destroys the evidence (an operator
can no longer see the upstream emitted a bad value) and **inverts**
meaning: SeverityNumber is monotonic
([logs/data-model/#severity-fields](https://opentelemetry.io/docs/specs/otel/logs/data-model/#field-severitynumber)),
so `25` is "more severe than FATAL4 (24)", and clamping it to `0` turns the
*most* severe record into the *least*-informative one. It is also doubly
damaging because `severity_number` is a template-key component
(`(severity_number, scope_name)`, `crates/ourios-miner/src/cluster.rs:1680`):
every out-of-range value collapses into the single UNSPECIFIED bucket,
co-mingling distinct severities in mining.

Change to **preserve verbatim**:

- `0..=24` (defined) and `25..=255` (out of the named ranges but storable
  and monotone-meaningful) → stored as the wire value;
- a record with `severity_number` outside `0..=24` is recorded on the
  existing `ourios.ingest.records` counter with the standard **`error.type`**
  attribute set to `severity_out_of_range` — the OTel "recording errors on
  metrics" convention (one counter for success + anomaly, reason on a
  low-cardinality `error.type`; success records carry no `error.type`), not
  a bespoke counter. `severity_text` is retained, so the violation is
  observable, not masked;
- the values a `u8` physically cannot hold (negative, `> 255`) become `0` —
  here the storage invariant wins (§3.0 point 1's limit). Because they
  narrow to `0`, they are indistinguishable post-narrowing from a genuine
  UNSPECIFIED and so are **not** separately attributed on the counter (an
  accepted limitation: such values are degenerate corruption, not a
  meaningful severity); the `25..=255` case — the one an operator actually
  sees — is fully attributed.

Severity comparisons (RFC 0002, which correctly compares on
`SeverityNumber`) stay monotone and correct: `severity >= ERROR` still
matches a `25`. The `u8` column is retained: `0..=255` covers the entire
defined range with 10× headroom for any conceivable future OTLP expansion,
and the only values it cannot represent (negative / `> 255`) are
definitionally garbage with nothing to preserve. Widening the column to
`i32` for absolute wire-fidelity is a one-line alternative (§7).

### 3.6 Correct the `body` column documentation (RFC 0005)

RFC 0005 §3.2 describes the `body` column as "raw bytes … not text," but
for `body_kind = Structured` rows it holds **UTF-8 canonical JSON** (§3.3).
Clarify the column note: raw original bytes for retained `String` rows;
UTF-8 canonical-JSON `AnyValue` for `Structured` rows; absent on clean
`String` rows. Documentation-only; no schema change.

## 4. Alternatives considered

**Defer everything except the MUST (§3.1).** Tempting — §3.1 is the only
flat MUST. Rejected per the maintainer's "one compliance push": §3.2 is a
real data-loss bug and the rest are cheap, so clearing them together avoids
a second disruptive amendment to the same files.

**Add `scope_attributes` as typed columns rather than canonical JSON.**
Rejected — it would diverge from how `attributes` / `resource_attributes`
are already stored (canonical JSON, RFC 0005 §3.3) for no benefit; the
typed-attribute representation is a separate, deferred RFC 0005 question.

**Keep `INTERNAL` and rely on clients retrying anyway.** Rejected — the
OTLP retry table is normative; compliant clients treat `INTERNAL` as
non-retryable and drop the batch. Relying on non-compliant client
behaviour is not fidelity.

**Make `event_name` queryable only via the generic attribute path.**
Rejected — `event_name` is a top-level LogRecord field, not an attribute;
it deserves a bare field like `severity` / `scope`, and forcing
`attr.event_name` would misrepresent the data model.

**One amendment RFC per touched RFC (three RFCs).** Rejected per the chosen
sequencing; a single RFC keeps the cross-cutting fidelity story coherent
and the acceptance scenarios in one place. Each touched RFC gets a
back-reference.

## 5. Acceptance criteria

> **Scenario RFC0018.1 — scope attributes + schema URLs survive ingest→storage**
> - **Given** an OTLP batch whose `InstrumentationScope` carries
>   `attributes`, whose `ScopeLogs` carries a `schema_url`, and whose
>   `ResourceLogs` carries a `schema_url`
> - **When** the receiver materialises the records and they are written
>   to Parquet
> - **Then** `scope_attributes` (canonical JSON), `scope_schema_url`,
>   and `resource_schema_url` are persisted with the wire values, and a
>   round-trip read returns them unchanged

> **Scenario RFC0018.2 — the new columns are OPTIONAL / back-compatible**
> - **Given** a historical Parquet file written before this amendment
>   (no `scope_attributes` / `*_schema_url` columns)
> - **When** the reader opens it
> - **Then** it reads successfully, the three fields read as absent/NULL,
>   and no error is raised (RFC 0005 §3.5 migration rule)

> **Scenario RFC0018.3 — transient ingest failure is reported retryable**
> - **Given** a WAL append/fsync failure during an Export call
> - **When** the receiver responds
> - **Then** the gRPC status is a **retryable** code (`UNAVAILABLE`, or
>   `RESOURCE_EXHAUSTED` + `RetryInfo`) and the HTTP status is `503`
>   (or `429`) — never `INTERNAL` / `500`
> - **And** a permanent failure (malformed payload, tenant resolution)
>   still maps to `INVALID_ARGUMENT` / `400`

> **Scenario RFC0018.4 — `event_name` is filterable in the DSL**
> - **Given** stored rows with differing `event_name` values
> - **When** a DSL query filters on `event_name`
> - **Then** the predicate compiles to the `event_name` column and
>   returns exactly the matching rows, with no DataFusion/SQL surface
>   leaking to the user (H6)

> **Scenario RFC0018.5 — non-finite doubles round-trip through canonical JSON**
> - **Given** an `AnyValue` (body or attribute) containing `NaN`,
>   `Infinity`, and `-Infinity`
> - **When** it is canonical-encoded and decoded
> - **Then** the decoded value equals the original (no `null` collapse)

> **Scenario RFC0018.6 — out-of-range SeverityNumber is preserved, not clamped (§3.0)**
> - **Given** OTLP records with `severity_number = 25` and `= 200`
>   (out of the named ranges but `u8`-storable)
> - **When** the receiver materialises them
> - **Then** the stored `severity_number` is `25` / `200` verbatim
>   (never silently clamped to `0`), the `ourios.ingest.records` counter
>   records them with `error.type = severity_out_of_range`, and a
>   `severity >= ERROR` query still matches them (monotonicity preserved)
> - **And** a value a `u8` cannot hold (negative, `> 255`) maps to `0`
>   (the storage invariant, not a correction); narrowed to `0`, it is not
>   separately attributed (the §3.5 accepted limitation)

## 6. Testing strategy

- **RFC0018.1 / .2** — an ingester→parquet integration test asserting the
  three new fields round-trip (incl. a non-empty `scope_attributes` decoded
  to structured kv); a reader test over a fixture file lacking the columns
  (back-compat). `scope_attributes` reuses the `ourios-core` canonical
  encode/decode **property tests**.
- **RFC0018.3** — receiver unit tests injecting a transient WAL failure
  (gRPC → retryable code; HTTP → 503/429) and a permanent failure
  (INVALID_ARGUMENT / 400), mirroring the existing RFC0003.4 mapping tests.
- **RFC0018.4** — a DSL parse+compile test for `event_name` filters plus an
  end-to-end querier test asserting matched rows; the H6 no-leak guard
  (RFC0007.3 style) extended to the new field.
- **RFC0018.5** — a **property test** over `AnyValue` including non-finite
  doubles, replacing the current "encodes to null" assertion with a
  round-trip one.
- **RFC0018.6** — a receiver test feeding `severity_number` 25 and 200 and
  asserting they are **preserved** (not clamped), that the
  `ourios.ingest.records` counter records them with
  `error.type = severity_out_of_range` (in-memory `MeterProvider`, mirroring
  the compaction-metric test), and that a `severity >= ERROR` query still
  matches them (monotonicity); plus a negative / `>255` case asserting `0`
  (the storage-invariant limit). Replaces the prior clamp-to-0 assertion in
  `severity_to_u8`'s tests (a contract change — the old test asserted the
  behaviour this RFC overturns; CLAUDE.md §6.2).

Each scenario id (`RFC0018.N`) is referenced from its test so the mapping
is greppable (`docs/verification.md` §2).

## 7. Open questions

- [ ] **Saturation backpressure depth** — §3.2 makes transient failures
  retryable, but a real rate-limiter / queue-depth signal (429 with a
  computed `Retry-After`) is still deferred (RFC 0003 §6.7). Land the code
  mapping now; size the limiter later?
- [x] **`scope_attributes` as a template-key input?** — *resolved: stay out
  of the key.* The key today is `(severity_number, scope_name)`
  (`cluster.rs:1680`); `scope_version` is already retained-but-not-keyed, and
  `scope_attributes` follow that precedent. The keying principle: the key
  carries low-cardinality fields that identify the log statement's *semantic
  class* (`severity_number`, `scope_name`); higher-cardinality emitter
  *metadata* (`scope_version`, `scope_attributes`) is retained + queryable
  (`scope.<key>`) but not keyed — keying on it would explode `template_count`
  (the template-cardinality hazard; `CLAUDE.md` §3.1 / `docs/hazards.md` #1)
  for no fidelity gain (attributes are retained per-row;
  reconstruction §3.3 never depended on scope). Per-attribute partitioning,
  if ever needed, is a future opt-in config (gated on a concrete consumer).
- [ ] **Pre-amendment backfill** — historical files lack the new columns;
  acceptable as NULL (best-effort) for pre-release, or backfill? (Leaning
  acceptable, consistent with the `effective_time_unix_nano` amendment.)
- [x] **`SeverityNumber` reject vs clamp** — *resolved: preserve + flag,
  neither reject nor clamp* (§3.0 / §3.5). The faithful-witness principle
  settles it: clamping is a silent correction, rejecting is silent data
  loss; both are the backend overstepping a role that belongs upstream.
- [x] **Severity column `u8` vs `i32`** — *resolved: `u8`.* `0..=255` covers
  the defined `1..=24` with 10× headroom for any conceivable future OTLP
  expansion; the only values it cannot hold (negative, `> 255`) are
  definitionally garbage with nothing meaningful to preserve, so they take
  the §3.5 storage-invariant path (`0` + anomaly count).
- [ ] **Anomaly visibility on read** — §3.5 surfaces out-of-range severity
  via a metric; should the read path (`LogRow`, RFC 0017) also mark a record
  as carrying out-of-spec severity, so it's visible per-record and not only
  in aggregate? (Leaning a metric for now; per-record marker if operators
  ask.)

## 8. References

- OTLP/OTel spec: [logs data model](https://opentelemetry.io/docs/specs/otel/logs/data-model/)
  (field set, severity fields), [common/#anyvalue](https://opentelemetry.io/docs/specs/otel/common/#anyvalue)
  (empty/zero MUST be stored), [common/instrumentation-scope](https://opentelemetry.io/docs/specs/otel/common/instrumentation-scope/)
  (the `(name,version,schema_url,attributes)` tuple),
  [otlp/#json-protobuf-encoding](https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding)
  (proto3 JSON mapping — non-finite doubles as `"NaN"`/`"Infinity"`/`"-Infinity"`
  strings),
  [otlp/#failures](https://opentelemetry.io/docs/specs/otlp/#failures)
  (retryable vs non-retryable codes), [otlp/#otlpgrpc-throttling](https://opentelemetry.io/docs/specs/otlp/#otlpgrpc-throttling).
- RFCs amended: **RFC 0002** (DSL — §6.1 bare fields), **RFC 0003**
  (receiver — §6.1/§6.2 error mapping, §6.6 materialisation, §6.8/§9 the
  previously-deferred schema_url + scope attributes), **RFC 0005** (schema —
  §3.2 columns, §3.3 canonical encoding, §3.5 migration). Consumed by **RFC
  0017** (`LogRow` gains the complete scope) and **RFC 0010** (drift over
  scope `schema_url`).
- `CLAUDE.md` §3.5 (schema migration — additive OPTIONAL columns), §3.7
  (multi-tenancy — new columns per-tenant), hazard H6 (no DataFusion leak),
  §3.3/§3.4 (the durability the retry-mapping fix protects).
- Code: `crates/ourios-ingester/src/receiver/materialize.rs` (scope/schema
  drop), `grpc.rs` / `http.rs` (error mapping), `crates/ourios-core/src/otlp.rs`
  (canonical encoder; severity decode), `crates/ourios-parquet/src/lib.rs`
  (schema), `crates/ourios-querier/src/dsl/ir.rs` (the `Field` enum).
