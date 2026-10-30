---
rfc: 0061
title: OTLP request-size limit
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-10-31
supersedes: —
superseded-by: —
---

# RFC 0061 — OTLP request-size limit

> **Status: `drafted`.** Tracks #1132. This RFC **amends RFC 0018 §3.2**
> (`accepted`): an oversize payload's gRPC code changes from
> `INVALID_ARGUMENT` to `RESOURCE_EXHAUSTED`. The HTTP side keeps its
> `413`. RFC 0003 RFC0003.11 already lists `RESOURCE_EXHAUSTED` for "an
> over-size request body exceeding the receiver's configured request-size
> limit", so this RFC also gives that criterion the configured limit it
> refers to. RFC 0018 §3.2 gets a dated pointer here once this RFC is
> accepted.

## 1. Summary

Both OTLP listeners enforce a single configurable request-size limit,
`receiver.max_request_bytes`. Its default is the WAL frame ceiling,
16 MiB. A gRPC Export over the limit, whether it is over before or after
decompression, gets `RESOURCE_EXHAUSTED` with no `RetryInfo`, which the
OTLP spec defines as non-retryable. An HTTP Export over the limit gets
`413`, as it does today. A batch that passes the limit but still exceeds
the WAL frame (`AppendError::TooLarge`) gets the same codes. Every
rejection is counted on `ourios.ingest.batches` with
`error.type = request_too_large`.

## 2. Motivation

[OTLP 1.11.1 §OTLP/gRPC Request](https://opentelemetry.io/docs/specs/otlp/#otlpgrpc-request):

> The server MUST enforce a message size limit when receiving the
> request, including after decompression […] it is RECOMMENDED to use
> 64 MiB as the default limit. Implementations SHOULD allow this limit to
> be configured. If the limit is exceeded, the gRPC server
> implementations MUST report a `RESOURCE_EXHAUSTED` code as a
> non-retryable error.

Today the gRPC listener (`ourios-server/src/receiver.rs`,
`LogsServiceServer::new(...)`) sets no limit, so tonic 0.14's default
applies: 4 MiB, enforced in two places with two different codes
(`tonic/src/codec/decode.rs`):

- A frame whose declared length is over the limit (any identity-encoded
  body over 4 MiB, or a gzip body whose *compressed* length is over it)
  is rejected with **`OUT_OF_RANGE`**.
- A gzip body whose *decompressed* length goes over it is rejected with
  `RESOURCE_EXHAUSTED`.

`OUT_OF_RANGE` is **retryable** in the
[OTLP failures table](https://opentelemetry.io/docs/specs/otlp/#failures).
A client that sends an oversize batch therefore resends the identical
request with backoff until its export timeout runs out, then drops it.
That is wasted load on the receiver plus data loss whose cause the client
never learns. The 4 MiB default is also below what a stock Collector
batch can reach, and operators cannot raise it.

Separately, RFC 0018 §3.2 maps the WAL's `AppendError::TooLarge` (a
batch over `ourios_wal::MAX_FRAME_BYTES`, 16 MiB) to `INVALID_ARGUMENT`.
That code is non-retryable, so behaviour was correct, but the spec now
names `RESOURCE_EXHAUSTED` for exactly this case. One condition, "the
request is too big", should have one code.

The HTTP listener already conforms (`413`, with the decompressed size
checked; [OTLP §OTLP/HTTP Request](https://opentelemetry.io/docs/specs/otlp/#otlphttp-request)).
Its 4 MiB default (`receiver/http.rs`, `HttpConfig::max_body_bytes`) is
not on the server config surface, though.

## 3. Proposed design

### 3.1 One limit, on the config surface

`receiver.max_request_bytes` (YAML, with `${env:VAR}` substitution like
every `ReceiverSection` field) sets the maximum Export body size in
bytes. The limit is measured after decompression, and also on the wire
bytes. The value is shared by both listeners.

- **Default `16777216` (16 MiB) = `ourios_wal::MAX_FRAME_BYTES`.** One
  Export becomes one WAL frame (`TenantBatch::encode(tenant,
  request.encode_to_vec())`, `receiver/pipeline.rs`), so a larger limit
  would only move the rejection from the listener to the WAL append,
  after the whole request had been decoded into memory. This is a
  documented deviation from the spec's RECOMMENDED 64 MiB default.
  RECOMMENDED is a SHOULD-level clause; §4 says why the frame ceiling is
  not raised to meet it.
- **Validation:** `1 ..= MAX_FRAME_BYTES`. Zero, or a value over the
  frame ceiling, is a config error at startup that names the field and
  the ceiling. It is never silently clamped.
- This is a transport bound, not an invariant knob (RFC 0004 §3.3). It
  bounds memory per request and touches no §3 invariant. The WAL's frame
  ceiling stays the hard backstop.

### 3.2 gRPC: `RESOURCE_EXHAUSTED`, never `OUT_OF_RANGE`

- The service is built with `max_decoding_message_size(limit)`. Both of
  tonic's checks then use the configured limit.
- tonic's frame-length check still emits `OUT_OF_RANGE`. A tower layer
  directly around `LogsServiceServer` rewrites a response whose
  `grpc-status` is `11` (`OUT_OF_RANGE`) to `8` (`RESOURCE_EXHAUSTED`)
  and keeps tonic's `grpc-message`. This is sound because the
  `LogsReceiver` handler never returns `OUT_OF_RANGE` (its mapping is the
  closed list in `receiver/grpc.rs`), so on this service an
  `OUT_OF_RANGE` can only come from the decoder's size check. tonic
  sends a decode failure as a trailers-only response, so the status
  arrives in the response headers and the layer needs no body or
  trailer handling.
- `AppendError::TooLarge` → `RESOURCE_EXHAUSTED` (was `INVALID_ARGUMENT`).
- No over-limit response carries `RetryInfo`, so by the OTLP table the
  client treats it as non-retryable and drops the batch. This keeps it
  distinct from backpressure: RFC 0053 §3.1 signals backpressure with
  `UNAVAILABLE`, and with `RetryInfo` when a delay is known. It does not
  use `RESOURCE_EXHAUSTED`, so a bare `RESOURCE_EXHAUSTED` from Ourios
  always means "too big, do not resend".

### 3.3 HTTP: unchanged `413`, shared limit

`HttpConfig::max_body_bytes` is set from `receiver.max_request_bytes`
(its default rises from 4 MiB to 16 MiB). The `DefaultBodyLimit` and
gunzip-bound paths and their `413` stay as they are, and `TooLarge`
stays `413` (RFC 0018 §3.2, unchanged for HTTP).

### 3.4 Amendment to RFC 0018 §3.2

The permanent-failure bullet's oversize clause becomes:

> an **oversize payload** (over `receiver.max_request_bytes`, RFC 0061,
> or `AppendError::TooLarge`) → HTTP `413` (gRPC `RESOURCE_EXHAUSTED`
> without `RetryInfo`, non-retryable per the OTLP failures table).

Nothing else in §3.2 changes. That includes RFC 0052's amendment (the
server-terminal, client-retryable class), which has no oversize case.

### 3.5 Telemetry

Every over-limit rejection, on either transport and from either the
listener or the WAL, adds 1 to the existing `ourios.ingest.batches`
counter with `error.type = request_too_large`. This uses the
`record_rejected_batch` path the auth rejections already use, so there
is no new instrument. `error.type` is the stable OTel attribute, and its
value space is open, so this adds no new name. The gRPC layer of §3.2
records it for the decoder's rejections, since the handler never runs
for them.

### 3.6 Invariants and hazards

- **§3.4 WAL-before-ack:** an over-limit request is rejected before any
  WAL append, so nothing is acked and nothing is written (RFC0003.11's
  "no `OtlpBatch` frame is appended"). The `TooLarge` path already
  rejects before ack.
- **§3.7 tenancy:** the limit is per request and the same for every
  tenant. Auth still runs before decode (RFC 0026), so an
  unauthenticated client cannot make the server buffer a body larger
  than the limit.
- **H3 (WAL durability):** unchanged. The frame ceiling is not touched.
- No other invariant or hazard is touched: no Parquet, miner or query
  change.

## 4. Alternatives considered

**Default to the RECOMMENDED 64 MiB and raise `MAX_FRAME_BYTES` to
match.** The frame ceiling is a reader-side cap in recovery. Raising it
changes what a WAL segment may contain, so a binary rolled back to an
older version could not replay a segment written by the newer one. That
is a persisted-layout change under RFC 0008, and the only gain is
accepting single batches over 16 MiB. That is in the range the Collector
itself caps its own OTLP/HTTP receiver at (`max_request_body_size`,
default 20 MiB), so 16 MiB already sits in the ecosystem's normal band.
Not worth a layout change for a SHOULD. If a real producer needs it, a later RFC can
split one large Export into several frames, which keeps the ceiling.

**Default to 64 MiB and let `TooLarge` reject anything over 16 MiB.**
This conforms on paper but advertises a limit the server cannot honour,
and it decodes up to 64 MiB into memory per request only to reject it.

**Leave tonic's `OUT_OF_RANGE` and only raise the limit.** Oversize
batches would still be retried until the export timeout. The retryable
code is the actual defect; the limit's size is secondary.

**A custom codec that enforces the limit itself.** That would avoid the
status rewrite, but it means owning the gRPC frame and decompression
code that tonic already ships correctly apart from the one status code.
The layer is a few lines, and RFC0061.2 pins tonic's behaviour, so a
tonic upgrade that changes it fails CI rather than drifting silently.

**Separate limits per transport.** Both carry the same
`ExportLogsServiceRequest` into the same WAL frame, so different limits
would only create a case where a batch is accepted over one transport
and rejected over the other.

## 5. Acceptance criteria

> **Scenario RFC0061.1 — the limit is configurable and validated**
> - **Given** `receiver.max_request_bytes` unset, set to `8388608`, set
>   to `0`, and set to `MAX_FRAME_BYTES + 1`
> - **When** the server resolves its config
> - **Then** the resolved limits are 16 MiB and 8 MiB for the first
>   two, and the last two fail startup with an error naming the field
>   and the ceiling

> **Scenario RFC0061.2 — an over-limit gRPC Export is non-retryable `RESOURCE_EXHAUSTED`**
> - **Given** a running gRPC listener with a configured limit `L`
> - **When** a client sends (a) an identity-encoded Export of `L + 1`
>   bytes, (b) a gzip Export whose compressed frame is over `L`, and
>   (c) a gzip Export under `L` compressed that decompresses past `L`
> - **Then** each response is `RESOURCE_EXHAUSTED` with no `RetryInfo`
>   detail (never `OUT_OF_RANGE`), no WAL frame is appended, and the
>   process stays up
> - **And** `ourios.ingest.batches{error.type="request_too_large"}`
>   rises by 3

> **Scenario RFC0061.3 — a batch over the WAL frame maps the same way**
> - **Given** an `AppendError::TooLarge` from the journal
> - **When** the gRPC and HTTP mappers handle it
> - **Then** gRPC returns `RESOURCE_EXHAUSTED` without `RetryInfo`, HTTP
>   returns `413`, and `error.type="request_too_large"` is recorded

> **Scenario RFC0061.4 — at-limit Exports are accepted on both transports**
> - **Given** a configured limit `L`
> - **When** a gRPC and an HTTP Export just under `L` (and under the WAL
>   frame once tenant-wrapped) are sent
> - **Then** both are acked and their records are readable after flush

> **Scenario RFC0061.5 — HTTP uses the shared limit**
> - **Given** `receiver.max_request_bytes = L`
> - **When** an HTTP Export over `L`, plain and gzip, is sent
> - **Then** each gets `413` (RFC0003.11's HTTP arm, now driven by the
>   configured `L` instead of the hard-coded 4 MiB)

## 6. Testing strategy

- RFC0061.1: config resolution unit tests in `ourios-server/src/config`,
  next to the existing `ReceiverSection` substitution tests.
- RFC0061.2, .4, .5: integration tests in `ourios-ingester/tests/it`
  (beside `http_transport_errors.rs`) driving a real tonic client and
  the axum router. The three gRPC arms of .2 also extend RFC0003.11's
  test. Arm (a) is what pins tonic's `OUT_OF_RANGE` path behind the
  rewrite layer.
- RFC0061.3: the `receiver/grpc.rs` and `receiver/http.rs` mapping unit
  tests. The existing `oversize_payload_is_invalid_argument` assertion
  changes to `RESOURCE_EXHAUSTED`. That is the contract change this RFC
  makes (AGENTS.md §6.2), so it is made only once this RFC is approved.
- No benchmark: nothing on the ingest hot path changes, apart from a
  header check on error responses.

## 7. Open questions

- [ ] Is a 16 MiB default right, or should it sit somewhat below the
  frame ceiling so that the tenant-wrap overhead never sends a
  just-under-limit batch to the `TooLarge` path? The client sees the
  same code either way, so this is cosmetic. Leaning toward 16 MiB for a
  round, documented number.
- [ ] Should the Helm chart surface `receiver.max_request_bytes`, or is
  `extraEnv` + `${env:VAR}` enough (the standing chart convention)?
  Leaning toward the latter.

## 8. References

- OTLP 1.11.1: [OTLP/gRPC Request](https://opentelemetry.io/docs/specs/otlp/#otlpgrpc-request),
  [Failures](https://opentelemetry.io/docs/specs/otlp/#failures),
  [OTLP/HTTP Request](https://opentelemetry.io/docs/specs/otlp/#otlphttp-request)
- RFC 0003 §5 RFC0003.11 (transport-level errors); RFC 0018 §3.2
  (amended here); RFC 0052 (its §3.2 amendment, unaffected); RFC 0053
  §3.1 (backpressure uses `UNAVAILABLE`); RFC 0008 (WAL frame ceiling,
  unchanged); RFC 0026 (auth before decode); RFC 0004 §3.3 (config
  classification)
- `AGENTS.md` §3.4, §3.7, §6.2
- Issue #1132
