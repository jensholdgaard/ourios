---
rfc: 0069
title: Reading back every nested value ingest accepts
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-11-20
supersedes: —
superseded-by: —
---

# RFC 0069 — Reading back every nested value ingest accepts

> **Status: `drafted`.** Tracks #1136, options A and C, which the
> maintainer chose on 2026-11-20; option B, ingest conformance at the
> SDK's depth of 64, is a follow-up (§4.1). This RFC changes the read
> path's contract for stored `AnyValue` JSON. It contradicts no accepted
> text: RFC 0001 §6.6's render contract already allows the retained
> `body` bytes returned verbatim, which §3.3 relies on.

## 1. Summary

The read path decodes the canonical JSON Ourios stores for structured
bodies, log attributes and resource attributes with `serde_json`'s
fixed recursion limit of 128, which is lower than the depth some values
ingest accepts need. So Ourios acknowledges values it cannot read back.
This RFC makes the read side decode any depth ingest can admit, with an
explicit, stack-safe bound, and makes a row that still fails to decode
degrade on its own instead of failing its whole batch, query or
compaction.

## 2. Motivation

#1136 measured the first nesting depth that fails on each path. Over
protobuf, `prost`'s recursion limit of 100 messages admits arrays
nested up to 49 deep, and maps up to 33 deep in a body or resource
attribute. The canonical JSON decode fails from 43 array layers and 33
map layers, because each map layer costs four JSON levels and each
array layer three. Such a value is acknowledged, logged to the WAL and
written to Parquet, then:

- `batch_to_mined_records` returns `ReaderError::AttributeDecode` for
  the whole batch, and every query whose result includes the row fails
  with `decode rows: …`;
- compaction fails on that file every pass, so the partition never
  compacts (H4);
- a structured body that does not decode renders as an empty line
  labelled `RetainedVerbatim`, which claims a fidelity it does not have.

That breaks `AGENTS.md` §3.3 and the read path's promise to return every
OTLP field ingest stored.

## 3. Proposed design

### 3.1 Read depth covers ingest depth

The canonical decoders `decode_any_value` and `decode_attributes`
(`ourios-core/src/otlp.rs`) lift `serde_json`'s fixed limit
(`Deserializer::disable_recursion_limit`, behind `serde_json`'s
`unbounded_depth` feature, which adds no crate) and bound depth
themselves:

- before parsing, a scan of the bytes counts JSON nesting, ignoring
  brackets inside strings, and refuses input deeper than
  `MAX_READ_DEPTH` with `CanonicalJsonError::TooDeep`;
- `MAX_READ_DEPTH` is **256** JSON levels. The deepest value ingest can
  admit is bounded by `prost`'s 100-message limit and `serde_json`'s
  128-level OTLP/JSON limit: at most 50 array layers of three levels
  or 33 map layers of four, about 150 levels. 256 covers that with
  margin, and a test pins the relationship (RFC0069.2), so a later raise
  of an ingest limit fails a test rather than reopening #1136;
- recursion through 256 levels must fit the stacks the query and
  compaction threads run on; RFC0069.2 decodes a value at
  `MAX_READ_DEPTH` on a thread with the smallest of those stacks.

`serde_json`'s fixed limit only ever served as a stack guard, and the
scan now does that job, so the guard is still there.

### 3.2 A row that does not decode degrades on its own

A row whose stored JSON still fails to decode, from file corruption, a
foreign producer, or a value deeper than `MAX_READ_DEPTH`, no longer
fails its batch:

- **Structured body.** `render_log_body` returns the stored body bytes
  as the line under `Reconstruction::RetainedVerbatim`, which is exactly
  what RFC 0001 §6.6's render contract means by that signal, instead of
  an empty line under the same label.
- **Attributes.** The row's decoded attributes are replaced by a single
  attribute whose key is reserved (§7) and whose string value is the
  stored canonical JSON, so the data stays visible and the row says why
  it is not structured.
- **Batch and query.** `batch_to_mined_records` decodes the other rows
  of the batch as before. A query returns every row, the degraded one
  marked as above.
- **Compaction.** A degraded row is written back with its stored bytes
  unchanged, so compaction carries it forward byte for byte and the
  partition compacts.

Each degraded row logs one `WARN` event naming the file and row, and is
counted (§7).

## 4. Alternatives considered

### 4.1 B: one explicit ingest limit at the SDK's 64

Raise ingest to the OpenTelemetry SDK's `AttributeValueDepthLimit`
default of 64, with a bounded protobuf decoder in place of `prost`'s
all-or-nothing recursion limit, and reject a too-deep record through
`partial_success` instead of failing the whole export. That is the
conformance fix #1136 also asks for, and it is larger. It follows this
RFC as its own RFC, and §3.1's pinned relationship makes it raise
`MAX_READ_DEPTH` with it.

### 4.2 Fail closed on read, as today

Rejected: it leaves acknowledged data unreadable, and one row blocks a
whole query and a whole partition's compaction.

### 4.3 Unbounded read depth with a growing stack

`serde_stacker` would let the decode recurse without a limit by growing
the stack on the heap. It adds a dependency, and a stored value deeper
than any ingest admits is corruption, which §3.2 already handles, so an
explicit bound is enough.

## 5. Acceptance criteria

> **Scenario RFC0069.1 — every depth ingest accepts reads back `[§3.3, H7]`**
> - **Given** a map and an array chain at every depth ingest accepts,
>   over protobuf and OTLP/JSON, as a body, a log attribute and a
>   resource attribute
> - **When** each is ingested, written to Parquet and read back by a
>   query
> - **Then** the value read back equals the value sent

> **Scenario RFC0069.2 — the read bound covers the ingest bound**
> - **Given** the deepest value each ingest path accepts
> - **When** its canonical JSON depth is measured
> - **Then** it is at most `MAX_READ_DEPTH`, a value at
>   `MAX_READ_DEPTH` decodes on a thread with the smallest stack the
>   query and compaction paths use, and a value one level past it is
>   refused with `CanonicalJsonError::TooDeep`, without overflowing the
>   stack

> **Scenario RFC0069.3 — an undecodable row degrades alone `[§3.3, H4]`**
> - **Given** a Parquet file holding one row whose structured body and
>   attributes are invalid canonical JSON, among valid rows
> - **When** a query reads the file, and when compaction rewrites it
> - **Then** the query returns every row, the bad row's body as its
>   stored bytes under `RetainedVerbatim` and its attributes as the
>   reserved attribute holding the stored JSON; compaction succeeds and
>   the bad row's stored bytes are unchanged in the output; one `WARN`
>   event names the file and row

## 6. Testing strategy

- RFC0069.1: an integration test in `ourios-server` that ingests the
  depth matrix of #1136 over both transports and queries it back,
  beside RFC 0017's read-path tests.
- RFC0069.2: unit tests in `ourios-core` beside the canonical JSON
  round-trip tests, building the deepest admitted value from the ingest
  limits rather than a hard-coded number.
- RFC0069.3: reader and compaction tests in `ourios-parquet`, and a
  render test in `ourios-querier` replacing
  `structured_undecodable_body_is_retained_not_faithful`'s expectation
  of an empty line, as a superseded test kept `#[ignore]`d beside its
  successor.
- `ourios-testgen`'s `MAX_ANY_VALUE_DEPTH` of 16 stays for the property
  suites; the depth matrix is explicit tests.

## 7. Open questions

- [ ] **The reserved attribute key (semconv sign-off).** Proposed
  `ourios.attribute.undecodable` (string), holding the stored canonical
  JSON of a row's attributes that did not decode. It goes through the
  shared `ourios-semconv` registry, checked against the OTel
  conventions.
- [ ] **The degraded-row signal.** Proposed: the event
  `ourios.reader.row.undecodable` (`WARN`, with `file.path` and the row
  index), and a count of degraded rows, on an existing reader counter
  with `error.type` `undecodable` if one fits, otherwise a new one.
  Both are registry names needing sign-off.

## 8. References

- #1136, the measurements and options; #1134, where it was found.
- RFC 0001 §6.6 (render contract, `RetainedVerbatim`), RFC 0005 (the
  stored canonical JSON), RFC 0017 (read path), RFC 0018 §3.4
  (non-finite doubles in the same decoders).
- OpenTelemetry common spec, `AnyValue` and attribute limits.
- `AGENTS.md` §3.3; `docs/hazards.md` H4 and H7.
