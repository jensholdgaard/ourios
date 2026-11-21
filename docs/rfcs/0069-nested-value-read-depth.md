---
rfc: 0069
title: Reading back every nested value ingest accepts
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-11-20
supersedes: —
superseded-by: —
---

# RFC 0069 — Reading back every nested value ingest accepts

> **Status: `specified`** (2026-11-21, maintainer-approved, with the
> §7 names). Tracks #1136, options A and C, which the
> maintainer chose on 2026-11-20; option B, ingest conformance at the
> SDK's depth of 64, is a follow-up (§4.1). This RFC **amends accepted
> RFC 0017 §3.3–§3.4**, which require a structured body and decoded
> attributes on every row with only an absent-body fallback: §3.2 adds
> the undecodable-value exceptions. RFC 0017 gets a dated pointer here
> once this RFC is accepted. The fallback keeps RFC 0047 §3.4's masking
> guarantee (§3.2). It coordinates with RFC 0062 (`green`), whose §3.3
> fails a query on an unreadable cell (§3.2).

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
(`ourios-core/src/otlp.rs`), which serve the structured body and the
log, resource and scope attributes alike, lift `serde_json`'s fixed limit
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
fails its batch. Each of the four stored values, the structured body
and the log, resource and scope attributes, degrades **independently**:
one bad cell does not affect the row's other cells or other rows.

**In query output:**

- **Structured body.** `render_log_body` returns the stored body bytes
  as the line under `Reconstruction::RetainedVerbatim`, instead of an
  empty line under the same label. This is the exception to RFC 0017
  §3.3's structured-body rule.
- **Attributes.** An undecodable attribute cell is replaced by a single
  attribute whose key is reserved (§7) and whose string value is the
  stored canonical JSON, so the data stays visible and the row says why
  it is not structured. This is the exception to RFC 0017 §3.4's
  decoded attributes.
- **Masking.** The fallback carries the whole cell, so RFC 0047 §3.4's
  per-key masking cannot see inside it. A reader who is masked from any
  content key of the row's tenant gets the reserved attribute with its
  value masked as a whole, never the stored JSON, so no protected value
  leaks through a fallback.
- **Predicates.** A filter runs before the fallback, over the raw cell
  (`ourios_attr_cmp`, RFC 0062 §3.3). A predicate over an undecodable
  cell evaluates as SQL `NULL`, so it does not match, and the cell is
  logged as undecodable; the query no longer fails. The guard wraps the
  **whole** compiled comparison, outside any prefilter: a JSON-attribute
  comparison compiles to a `LIKE` prefilter `AND` the UDF, and a false
  prefilter would otherwise make the comparison false, so that `not`
  matches the undecodable row. Wrapped, the comparison is `NULL` and
  stays unmatched under `not` too. This changes RFC
  0062 §3.3's "a row the scan cannot read fails the query", which the
  implementing PR updates (RFC 0062 is `green`, so it is coordinated,
  not amended).
- **Batch and query.** `batch_to_mined_records` decodes the other cells
  and rows as before, and a query returns every row its predicates
  match.

**In compaction, nothing degrades.** The query fallback never reaches
the compaction path. A row with **any** undecodable cell is read
**whole** as its stored bytes, every cell of it raw, the valid ones
included, carried through sorting, spills and the final write in a
dedicated raw form, and written back unchanged, so the whole row is
carried forward byte for byte, not only its bad cell, and the partition
compacts. Rows that decode in full are compacted as today. That covers a body whose bytes are not valid
UTF-8 as well: compaction never routes a raw cell through a string
conversion.

**Erasure stays pending past an undecodable identity.** Erasure (RFC
0047 §3.6) runs through the same rewrite and finds the rows to erase by
the conversation id in their decoded attributes. A row whose attributes
cell does not decode has no trustworthy id, so the rewrite carries it
forward and the erasure for that partition **does not advance its
marker**: it stays pending, the row is logged and counted, and the
erasure is not reported complete. Rows are never kept while the marker
claims they are gone, which keeps RFC 0047 §3.6's rows-before-tuples
guarantee.

Each degraded cell in a query logs one `WARN` event naming the file,
row and column, and is counted (§7).

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
>   over protobuf and OTLP/JSON, as a body, a log attribute, a resource
>   attribute and a scope attribute
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

> **Scenario RFC0069.3 — an undecodable cell degrades alone `[§3.3, H4]`**
> - **Given** a Parquet file holding, among valid rows, one row whose
>   structured body, log attributes, resource attributes and scope
>   attributes are each invalid canonical JSON, and another row whose
>   scope attributes alone are, with a body that is not valid UTF-8
> - **When** a query reads the file
> - **Then** it returns every row: the bad body as its stored bytes
>   under `RetainedVerbatim`, each bad attribute cell as the reserved
>   attribute holding its stored JSON, and every valid cell of those
>   rows decoded as usual; one `WARN` event names each bad cell

> **Scenario RFC0069.4 — compaction carries an undecodable cell byte for byte `[H4]`**
> - **Given** RFC0069.3's file
> - **When** compaction rewrites its partition, including a run large
>   enough to spill
> - **Then** compaction succeeds, every bad cell's stored bytes are
>   unchanged in the output, the non-UTF-8 body included, and no output
>   row carries the reserved attribute

> **Scenario RFC0069.5 — erasure stays pending past an undecodable identity `[§3.7]`**
> - **Given** a partition under an erasure request for a conversation,
>   holding a row whose attributes cell does not decode
> - **When** the erasure rewrite runs
> - **Then** the row is carried forward byte for byte, the erasure's
>   marker for that partition does not advance, the erasure is not
>   reported complete, and the row is logged

> **Scenario RFC0069.6 — the fallback is masked for a masked reader `[§3.7]`**
> - **Given** RFC0069.3's file, under a tenant with a content key masked
>   from a metadata-only reader (RFC 0047 §3.4)
> - **When** that reader queries it
> - **Then** the reserved attribute's value is masked as a whole, and the
>   stored JSON appears nowhere in the result

> **Scenario RFC0069.7 — a filter over an undecodable cell does not fail the query**
> - **Given** RFC0069.3's file, with valid rows matching an attribute
>   predicate and the bad row's attributes cell undecodable
> - **When** a query filters on that attribute
> - **Then** it returns the valid matching rows, the bad row does not
>   match, and the query does not fail
> - **And** the negated filter (`not (attr.k == "x")`), whose `LIKE`
>   prefilter is false for the bad cell, does not match the bad row
>   either

## 6. Testing strategy

- RFC0069.1: an integration test in `ourios-server` that ingests the
  depth matrix of #1136 over both transports and queries it back,
  beside RFC 0017's read-path tests.
- RFC0069.2: unit tests in `ourios-core` beside the canonical JSON
  round-trip tests, building the deepest admitted value from the ingest
  limits rather than a hard-coded number.
- RFC0069.3: reader tests in `ourios-parquet`, and a render test in
  `ourios-querier` replacing
  `structured_undecodable_body_is_retained_not_faithful`'s expectation
  of an empty line, as a superseded test kept `#[ignore]`d beside its
  successor.
- RFC0069.4: compaction tests in `ourios-parquet` beside RFC0009.2's,
  with a spill forced by a small memory budget.
- RFC0069.5: an erasure test in `ourios-ingester`'s compactor beside
  RFC 0047 §3.6's erasure tests.
- RFC0069.6: a visibility test in `ourios-querier` beside RFC 0047's
  masking tests.
- RFC0069.7: a plan test in `ourios-querier` beside RFC 0062's
  attribute-predicate tests.
- `ourios-testgen`'s `MAX_ANY_VALUE_DEPTH` of 16 stays for the property
  suites; the depth matrix is explicit tests.

## 7. Open questions

- [x] **The reserved attribute key.** Approved 2026-11-21:
  `ourios.attribute.undecodable` (string), holding the stored canonical
  JSON of a row's attributes that did not decode. It goes through the
  shared `ourios-semconv` registry, checked against the OTel
  conventions.
- [x] **The degraded-row signal.** Approved 2026-11-21: the event
  `ourios.reader.row.undecodable` (`WARN`, with `file.path` and the row
  index), and a count of degraded rows, on an existing reader counter
  with `error.type` `undecodable` if one fits, otherwise a new one.
  Both go through the shared `ourios-semconv` registry.

## 8. References

- #1136, the measurements and options; #1134, where it was found.
- RFC 0017 (`accepted`) §3.3–§3.4, **amended here**; RFC 0047
  (`accepted`) §3.4, whose masking §3.2 keeps; RFC 0001 §6.6
  (`RetainedVerbatim`); RFC 0005 (the stored canonical JSON); RFC 0018
  §3.4 (non-finite doubles in the same decoders).
- OpenTelemetry common spec, `AnyValue` and attribute limits.
- `AGENTS.md` §3.3; `docs/hazards.md` H4 and H7.
