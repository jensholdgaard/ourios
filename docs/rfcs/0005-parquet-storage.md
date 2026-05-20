---
rfc: 0005
title: Parquet storage — schema, writer, reader, audit stream
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-05-19
supersedes: —
superseded-by: —
---

# RFC 0005 — Parquet storage: schema, writer, reader, audit stream

## 1. Summary

Pins the on-disk Parquet contract that the `ourios-parquet` crate
implements. The contract has four parts: (a) the data-file schema —
a column-by-column mapping of RFC 0001 §6.1's record schema (the
planned `MinedRecord` Rust type — see §3.0) onto Parquet types,
with `tenant_id` and time as Hive-style partition keys; (b) the
audit-event file schema — a parallel file series
carrying the `TemplateWidened` / `TemplateTypeExpanded` /
`TemplateWideningRejectedDegenerate` records named in RFC 0001 §6.4;
(c) the writer's row-group / file sizing, compression codec, and
encoding policy, all anchored to `docs/hazards.md` H4 and the
`CLAUDE.md` §3.2 cardinality invariant; (d) the reader's forward-compatibility
contract (unknown columns ignored, missing columns surface as
documented defaults). Together these are the §3.5 schema baseline:
every column added after this RFC lands goes through an
incremental amendment, every column removed requires the §3.5
migration path.

## 2. Motivation

### 2.1 Phase 2 needs an RFC, not a stub crate

`docs/roadmap.md` §4 opens Phase 2 with one capability — "mined
records become Parquet files." `CLAUDE.md` §3.5 reads "All schema
changes go through the schema RFC process," and `docs/rfcs/README.md`
lists the on-disk Parquet schema in the "RFC required" set. A
`ourios-parquet` crate that lands without a schema RFC immediately
takes a schema commitment without going through the gate the
project's own rules require. RFC 0005 is that gate.

### 2.2 The schema is the contract with future data

Operators who run Ourios accrue Parquet files. A subsequent PR
that adds a non-OPTIONAL column, renames a column, or changes a
column's type breaks every reader that opens an older file — and
breaks every emitter against a deployment that hasn't upgraded.
Treating the schema as a written contract from PR-one forward
prevents the silent format drift that turns a working backend
into "redeploy and lose six months of logs." It is also what
makes `CLAUDE.md` §3.6 ("object storage is the source of truth")
durable: the truth has to be readable a year from now by code we
haven't written.

### 2.3 The Parquet pillar earns its compression here

Pillar 1 in `CLAUDE.md` §2 ("Parquet as the on-disk format") is
load-bearing for the thesis-gate A1 compression ratio. The
encoding decisions in this RFC — which columns dictionary-encode,
which carry bloom filters, which page indexes are enabled, what
the row-group target is, how `body` is *not* dictionary-encoded
because the `CLAUDE.md` §3.2 cardinality invariant forbids it — are where
A1's 50–200× promise gets paid. Pinning them in an RFC means
those decisions are reviewable independently of the writer's
implementation and stable across PRs that touch the writer for
unrelated reasons.

### 2.4 Why this is one RFC, not three

A natural split would be RFC 0005 (schema), RFC 0006 (writer),
RFC 0007 (reader, audit). Rejected: the schema, the writer's
sizing/encoding policy, and the reader's forward-compatibility
contract are co-designed. Splitting them into three RFCs
optimises for short documents but loses the cross-cutting
constraints (e.g. "no dictionary on `body`" is a schema rule
*and* a writer rule *and* a reader expectation). The RFC 0001
§6.8 telemetry surface and the eventual compaction policy are
real post-MVP concerns and get their own RFCs.

## 3. Proposed design

### 3.0 Terminology note

This RFC uses **`MinedRecord`** as the planned Rust type name for
the per-row record the miner emits, the same shape RFC 0001 §6.1
specifies but without yet naming a type. The §6.1 amendment uses
*"the record"* / *"the miner emits one record"*; this RFC chooses
`MinedRecord` for the type that backs the writer's input and the
reader's output, and uses it consistently below. A follow-on PR
to RFC 0001 may adopt the same name in §6.1; until then, treat
the two terms as synonyms.

### 3.1 Scope and what this RFC pins

This RFC pins:

- The Parquet logical schema (column names, types, repetition,
  nullability) for both the data-file series and the audit-event
  file series.
- The on-disk partition layout (Hive-style: `tenant_id=…/
  year=…/month=…/day=…/hour=…/`).
- The writer's row-group target, file-size target, compression
  codec, and per-column encoding policy (dictionary, page index,
  bloom filter).
- The reader's forward- and backward-compatibility contract.
- The `AnyValue` encoding rule for OTLP attribute and body
  payloads.
- The schema-evolution rules anchored to `CLAUDE.md` §3.5.

This RFC does **not** pin:

- Background compaction (deferred per `docs/roadmap.md` §4 Phase
  2 "Out of MVP scope, parked here" — a separate RFC after MVP).
- Query-engine plumbing (DataFusion table provider registration,
  predicate-pushdown wiring) — that's Phase 3 / RFC 0002
  territory.
- The wire-format receiver (gRPC / HTTP) — RFC 0003.
- The `body_shape_fingerprint` and `template_fingerprint` reserved
  extensions named in RFC 0001 §6.1 — those gate on "we have a
  concrete consumer."
- A typed Parquet representation of `AnyValue`'s `array` /
  `kvlist` branches — see §3.3 (rejected for MVP; future RFC).

### 3.2 Data-file Parquet schema

The mapping below is the normative column set. Field order is the
Parquet schema's declared order; readers MUST address columns by
name, not by ordinal.

**`tenant_id` is row-level, the partition path is an index over
it.** `tenant_id` is a **REQUIRED row-level column** in every
data file, listed in the schema table below. It is also replicated
as the leading Hive partition key (§3.4) so DataFusion / Arrow
can prune by tenant without opening files. Per
`docs/talks/0001-template-miner.md` ("`tenant_id` is present on
every row, not on every file ... we never trust the file to
tell us the tenant; we trust the row") the row-level value is
authoritative: the reader resolves `tenant_id` from the row,
treats the partition path as a partition-pruning index, and
**errors** on row-vs-path mismatch (§3.9). The time-bucket parts
(`year`, `month`, `day`, `hour`) are *pure-partition* pseudo-
columns derived from `time_unix_nano` rendered as UTC; they are
not stored row-level and their schema-evolution contract follows
§3.4 (the partition layout), not §3.8 (the row schema).

**Identity** (RFC 0001 §6.1 "Identity and partitioning"):

| Column | Parquet logical type | Physical type | Repetition | Notes |
|---|---|---|---|---|
| `tenant_id` | `STRING` | `BYTE_ARRAY` | REQUIRED | Authoritative tenant identifier; also replicated in the partition path (§3.4) for predicate-pushdown convenience. Row value wins on row-vs-path mismatch per §3.9 |
| `template_id` | `INTEGER(64, signed=false)` | `INT64` | REQUIRED | Monotonic; bloom-filter coverage (§3.6) |
| `template_version` | `INTEGER(32, signed=false)` | `INT32` | REQUIRED | Starts at 1; bumped on RFC 0001 §6.4 events |

**OTLP-derived columns** (RFC 0001 §6.1 "OTLP-derived columns"):

| Column | Parquet logical type | Physical type | Repetition | Notes |
|---|---|---|---|---|
| `time_unix_nano` | `TIMESTAMP(NANOS, isAdjustedToUTC=true)` | `INT64` | REQUIRED | `0` = unknown (OTLP convention); time partition key derived from this |
| `observed_time_unix_nano` | `TIMESTAMP(NANOS, isAdjustedToUTC=true)` | `INT64` | OPTIONAL | |
| `severity_number` | `INTEGER(8, signed=false)` | `INT32` | REQUIRED | OTLP `SeverityNumber` 0..24; part of template key |
| `severity_text` | `STRING` | `BYTE_ARRAY` | OPTIONAL | |
| `scope_name` | `STRING` | `BYTE_ARRAY` | OPTIONAL | Part of template key |
| `scope_version` | `STRING` | `BYTE_ARRAY` | OPTIONAL | |
| `attributes` | `STRING` (canonical JSON) | `BYTE_ARRAY` | REQUIRED | UTF-8 canonical JSON per §3.3 (mirrors RFC 0001's `Vec<KeyValue>` — always present, possibly empty). For a record with no attributes, the writer emits the canonical empty array `[]` (two bytes — repetitive across no-attribute records so ZSTD compression collapses it). `NULL` is not a valid encoding; the round-trip rule is `Vec::new()` ↔ `[]` |
| `dropped_attributes_count` | `INTEGER(32, signed=false)` | `INT32` | REQUIRED | Mostly zero |
| `resource_attributes` | `STRING` (canonical JSON) | `BYTE_ARRAY` | REQUIRED | Same contract as `attributes`: REQUIRED, UTF-8 canonical JSON, empty `Vec` ↔ `[]`, `NULL` not valid |
| `trace_id` | `UUID` | `FIXED_LEN_BYTE_ARRAY(16)` | OPTIONAL | Parquet's `UUID` logical type is the 16-byte binding for opaque identifiers; not interpreted as an RFC 4122 UUID |
| `span_id` | (no logical type) | `FIXED_LEN_BYTE_ARRAY(8)` | OPTIONAL | No matching Parquet logical type for 8-byte opaque ids; physical type alone is the contract |
| `flags` | `INTEGER(32, signed=false)` | `INT32` | REQUIRED | Lower 8 bits = W3C trace flags |
| `event_name` | `STRING` | `BYTE_ARRAY` | OPTIONAL | |

**Body and miner-derived columns** (RFC 0001 §6.1 "Body and
miner-derived reconstruction"):

| Column | Parquet logical type | Physical type | Repetition | Notes |
|---|---|---|---|---|
| `body_kind` | `INTEGER(8, signed=false)` | `INT32` | REQUIRED | `0` = `String`, `1` = `Structured` |
| `body` | `BYTE_ARRAY` | `BYTE_ARRAY` | OPTIONAL | Original bytes when retained per RFC 0001 §6.3 (lossy-zone retention) / RFC 0001 §6.5 (overflow forces retention); canonical-JSON `AnyValue` when `body_kind = Structured`; absent on clean-zone `String` rows. Intentionally no `STRING` logical type — the column carries raw bytes (potentially non-UTF-8 log lines or non-JSON binary), not text |
| `params` | `LIST<STRUCT<type_tag: INT32, value: BYTE_ARRAY>>` | as schema | REQUIRED | Always written (mirrors RFC 0001's `Vec<Param>`); the list is empty (zero elements) when `body_kind = Structured`. `NULL` is not a valid encoding |
| `separators` | `LIST<BYTE_ARRAY>` | as schema | REQUIRED | Always written (mirrors RFC 0001's `Vec<Separator>`); `tokens.len() + 1` elements when `body_kind = String`, zero elements when `body_kind = Structured`. `NULL` is not a valid encoding |
| `confidence` | `FLOAT` | `FLOAT` | REQUIRED | `1.0` sentinel when `body_kind = Structured` |
| `lossy_flag` | `BOOLEAN` | `BOOLEAN` | REQUIRED | Always `false` when `body_kind = Structured` |

`params`' nested struct uses the standard Parquet 3-level LIST
encoding (`list.element.<field>`); `separators` uses the same
3-level shape with `BYTE_ARRAY` elements. The `params.type_tag`
integer enum is `0..=7` matching RFC 0001's `ParamType` ordering:
`IP, UUID, NUM, HEX, TS, PATH, STR, OVERFLOW`. Adding a new
variant is a §3.5 schema amendment (additive, but readers MUST
know how to surface unknown variants — see §3.9).

### 3.3 `AnyValue` encoding rule

OTLP's `LogRecord.attributes` and `resource_attributes` are
`Vec<KeyValue>` where each value is an `AnyValue` discriminated
union (`string | bool | int | double | bytes | array | kvlist`).
Recursive (array, kvlist) variants do not map cleanly onto
Parquet's flat-nested schema — Parquet supports `LIST` and
`STRUCT` but the recursion depth has to be unrolled into the
schema declaration, which means no fixed-depth schema can
faithfully describe arbitrary `AnyValue` trees.

**Decision.** `attributes`, `resource_attributes`, and the
`body` column when `body_kind = Structured` are stored as a
single `BYTE_ARRAY` of **OTLP-canonical JSON** per RFC 0001 §6.1
("Canonical encoding for `body_kind = Structured`"). The
canonical encoding is the proto3 JSON mapping with OTLP's
specific overrides (`trace_id` / `span_id` as hex strings,
`bytes` as base64, etc.) — same rule for the three columns so
operators don't have to remember three encodings.

The rationale is on three legs:

1. **Faithfulness.** The canonical encoding is bidirectional —
   `stored_bytes ↔ AnyValue` round-trips deterministically (the
   normative `[§3.3]` reconstruction guarantee for the structured
   branch).
2. **Schema simplicity.** A single `BYTE_ARRAY` column versus a
   recursive `STRUCT<string_value, int_value, ...,
   array_value: LIST<...>, kvlist_value: LIST<STRUCT<...>>>`
   pseudo-schema with unrolled recursion depth.
3. **Query consumer absence.** Phase 3's thesis-gate B1/B2
   queries are predicate-pushdown on `template_id`, `tenant_id`,
   and `time_unix_nano` — none of those require typed AnyValue
   predicates. The typed-attribute query path is a future RFC
   gated on a concrete consumer.

A reserved future amendment may add a parallel typed-attribute
column set (likely a flattened `attributes_str: MAP<STRING,
STRING>` for the common `string`-valued case, leaving complex
values in the JSON column). The gate is "we have a concrete
consumer," not "it might be useful."

### 3.4 Partition layout on disk

Data files live at:

```
<bucket>/data/tenant_id=<tenant_id>/year=YYYY/month=MM/day=DD/hour=HH/<flush_uuid>.parquet
```

Audit-event files live at:

```
<bucket>/audit/tenant_id=<tenant_id>/year=YYYY/month=MM/day=DD/<flush_uuid>.parquet
```

The partition path segment is `tenant_id=` (not `tenant=`) so
the Hive-style partition-discovery convention (column name
= path segment key) resolves it to the same column name the
row-level schema declares; the reader's row-vs-path validation
(§3.9) compares values across the two surfaces unambiguously.

Where:

- `<tenant_id>` is the **percent-encoded** `TenantId` per
  RFC 3986 §2.1, with two project-specific overrides:
  - The input is the `TenantId`'s **UTF-8 byte sequence** (the
    `TenantId` newtype wraps a Rust `String`, which is already
    UTF-8). No Unicode normalisation is applied before encoding
    — the bytes are taken verbatim. This is deterministic and
    independent of the host's locale.
  - The **unreserved** set (`A-Za-z0-9`, `-`, `_`, `.`, `~`) is
    passed through unchanged. Every other byte is percent-encoded
    (`%XX` with upper-case hex digits). In particular `/` (path
    separator), `=` (partition key/value delimiter), and `%`
    (the escape introducer) are always escaped, regardless of
    whether RFC 3986 would treat them as reserved or unreserved
    in another context.
  - Decoding is the inverse; partition values that contain a
    malformed percent escape (e.g. `%XY` with non-hex digits)
    are a hard read error.
  Both writer and reader use this exact algorithm; the
  RFC0005.5 acceptance criterion's non-ASCII sub-test pins
  it.
- `year` / `month` / `day` / `hour` are derived from
  `time_unix_nano` rendered as UTC. Audit-event partitioning
  stops at `day=DD` because audit volume is far lower than data
  volume; an hour-level partition for audit would produce many
  tiny files for no win.
- **`time_unix_nano = 0` (OTLP "unknown" sentinel).** The
  writer derives the partition tuple by first checking
  `time_unix_nano`; if it is `0`, the writer falls back to
  `observed_time_unix_nano`. If `observed_time_unix_nano` is
  also absent or `0`, the record is placed under the epoch
  partition `year=1970/month=01/day=01/hour=00/` — operators
  see "unknown-time records cluster under 1970-01-01" as the
  documented signal, and an emitter-side investigation is the
  proper response. Rejecting the record was considered and
  rejected: §3.5 records are end-of-pipeline (the wire-decode
  receiver already accepted them), and a hard-reject here
  would silently drop data the WAL already acknowledged.
  Row-vs-path validation (§3.9) uses the same derivation
  rule, so a row at `time_unix_nano = 0` placed under the
  1970 partition validates cleanly.
- `<flush_uuid>` is the writer's flush identifier, **pinned to
  UUIDv7** (RFC 9562). UUIDv7 places a millisecond-precision
  Unix timestamp in its high bits, so files in a partition sort
  naturally by creation time when listed lexicographically.
  This is normative — the writer MUST emit UUIDv7. Operators
  inspecting a bucket can rely on sort-order = creation-order
  for tooling like "show me the latest file in this partition."

This is the **production** layout. The MVP corpus runner
(`ourios-bench` in Phase 3) is allowed to emit all records to a
single file under a degenerate partition path
(`tenant_id=corpus/year=2026/month=04/day=02/hour=10/`) because
corpus runs are bounded and producing 24 small files would
distract from the thesis-gate measurements. The H4 file-sizing
target (§3.5) is enforced on the production path; the corpus
path is exempt.

### 3.5 Row group, file size, compression codec

Anchored to `docs/hazards.md` H4 and the small-file-problem
detection threshold (file count must grow sub-linearly with
bytes ingested):

- **Row-group size target.** 128 MiB – 1 GiB **uncompressed**
  bytes per row group (binary units; the H4 target is written as
  "128 MB – 1 GB" but the operational detection threshold is in
  MiB, and Parquet byte counts in metadata are unprefixed binary
  bytes — RFC 0005 standardises on MiB/GiB throughout to avoid
  the ambiguity). The writer flushes a row group when its in-
  memory buffer crosses 128 MiB; row groups never exceed 1 GiB
  (the next row starts a new row group). Below 128 MiB only on
  the final row group of a file.
- **File size target.** 256 MiB – 2 GiB **compressed** bytes
  post-compaction. The writer's job is to land at the bottom
  of this range or below on its own (1024 MiB target
  uncompressed → typical 3–8× compression → ~128–340 MiB
  compressed file); compaction is deferred.
- **Compression codec.** **`ZSTD` level 3** for every column.
  ZSTD-3 is the Apache Arrow / DataFusion default and gives the
  best ratio-vs-throughput balance Ourios cares about; the
  thesis-gate A1 measurements will test whether the choice
  holds. Compression is orthogonal to per-column *encoding*
  (dictionary, RLE for booleans, RLE-encoded repetition /
  definition levels in `LIST` columns — all standard Parquet
  shapes that apply regardless of the chosen compression
  codec); §3.6 specifies the encoding policy.
- **Page size target.** Default 1 MiB pages (Arrow default).
  Bloom filters and page index live on a per-column basis
  (§3.6).

The targets are floors and ceilings, not exact numbers. A
writer flush forced by a time-based segment rotation (e.g.
producing the audit-event file at end-of-day) may emit a
small-row-group file; that's an acknowledged corner case the
compaction PR will sweep up. Steady-state production traffic
must produce files inside the §3.5 range; the H4 detection
metric ("fewer than 5 % of files below 128 MiB at steady
state") is the operational check.

### 3.6 Encoding policy

Per-column encoding decisions, anchored to query patterns
(thesis-gate B1/B2) and the `CLAUDE.md` §3.2 cardinality invariant:

| Column | Dictionary | Page index | Bloom filter | Rationale |
|---|---|---|---|---|
| `template_id` | yes | yes | **yes** | B2 (`where template_id = X`) is bloom-friendly; high cardinality but small relative to row count |
| `template_version` | yes | yes | no | Always small per template |
| `time_unix_nano` | no | yes | no | `DELTA_BINARY_PACKED` Parquet encoding (the writer's default for monotonic INT64 timestamps) plus ZSTD compression; min/max per page enables B1 range pruning |
| `severity_number` | yes | yes | no | 0..24 — dict alone is enough |
| `severity_text` | yes | yes | no | Bounded set in practice |
| `scope_name` | yes | yes | no | Bounded per deployment |
| `scope_version` | yes | yes | no | Bounded per deployment |
| `attributes` | no | no | no | JSON BYTE_ARRAY, high entropy, dict would balloon |
| `resource_attributes` | yes | no | no | Repetitive across rows of one tenant; dict pays |
| `trace_id` | no | yes | no | High cardinality, near-random — dict and bloom both lose |
| `span_id` | no | yes | no | Same |
| `flags` | yes | yes | no | Bounded |
| `event_name` | yes | yes | no | Bounded |
| `body_kind` | yes | yes | no | Two values |
| **`body`** | **no** | no | no | **`CLAUDE.md` §3.2 invariant: bodies are unbounded by design. Dictionary encoding would balloon — overflow is the safety valve, dict is the failure mode** |
| `params` (list values) | no | no | no | Per-row entropy too high |
| `separators` (list values) | yes | no | no | Almost always a single space — dict crushes it |
| `confidence` | no | yes | no | Float, narrow range, page-index sufficient |
| `lossy_flag` | n/a | yes | no | Boolean, RLE handles it |
| `dropped_attributes_count` | yes | yes | no | Almost always zero |

The `body` row is the only one with bold weight: a writer that
quietly enables dictionary encoding on `body` because Arrow's
default does so violates `CLAUDE.md` §3.2 ("Drain assumes
parameters are short, variable bits. Reality: a `params` slot
may capture an entire stack trace, request body, or base64
blob. Unbounded values destroy Parquet's dictionary encoding
and bloat files."). The RFC 0001 §6.5 OVERFLOW marker is the design
response in `params`; the `body` column is where retained
originals land, and those *are* unbounded by construction.

### 3.7 Audit-event file schema

The audit stream carries the events RFC 0001 §6.4 names —
`TemplateWidened`, `TemplateTypeExpanded`,
`TemplateWideningRejectedDegenerate` — plus a kind tag and a
timestamp. The contract from RFC 0001 §9 ("Cross-RFC contracts
pending") is fulfilled by this file series.

As in §3.2, `tenant_id` is a row-level REQUIRED column on the
audit record (also replicated as the leading Hive partition key,
§3.4); the time-bucket parts (`year`, `month`, `day`) are pure-
partition pseudo-columns derived from `timestamp`. The reader's
row-vs-path validation (§3.9) applies identically here.

**Event-kind mapping and dual-column storage.** RFC 0001 §6.4
refers to these audit events by snake_case `event_type` strings;
this RFC stores **both** an `event_kind` INT32 ordinal (compact,
dictionary-encodes to a few bytes) **and** an `event_type` STRING
column carrying the canonical RFC 0001 §6.4 string. The string
column is what RFC 0001 §9 names as the predicate-pushdown surface
for the RFC 0001 §6.7 drift query; the ordinal is what the writer and
reader use internally. Both columns are REQUIRED and the writer
must keep them in sync per the mapping table — divergence is an
implementation bug, not a degree of freedom. The normative
mapping:

| `event_kind` ordinal | `event_type` string (RFC 0001 §6.4) | Rust variant |
|---|---|---|
| `0` | `template_widened` | `TemplateWidened` |
| `1` | `template_type_expanded` | `TemplateTypeExpanded` |
| `2` | `template_widening_rejected_degenerate` | `TemplateWideningRejectedDegenerate` |

Adding a new ordinal is a §3.8 additive amendment; the mapping
table is the source of truth and a new ordinal lands as a new
row plus a new `event_type` string in the same PR. Renumbering
an existing ordinal or renaming an `event_type` string is
forbidden in-place (§3.8 rule 3: column-type changes go through
add-new-column / migrate / drop).

The row-level audit columns are:

| Column | Parquet logical type | Physical type | Repetition | Notes |
|---|---|---|---|---|
| `tenant_id` | `STRING` | `BYTE_ARRAY` | REQUIRED | Same contract as data-file `tenant_id`: row authoritative, replicated in partition path, mismatch → reader error |
| `timestamp` | `TIMESTAMP(NANOS, isAdjustedToUTC=true)` | `INT64` | REQUIRED | Cluster clock at emit time (matches RFC 0001 §6.4 `timestamp`) |
| `event_kind` | `INTEGER(8, signed=false)` | `INT32` | REQUIRED | Ordinal per the mapping table above |
| `event_type` | `STRING` | `BYTE_ARRAY` | REQUIRED | Canonical RFC 0001 §6.4 snake_case string; predicate-pushdown surface for the RFC 0001 §6.7 drift query |
| `template_id` | `INTEGER(64, signed=false)` | `INT64` | REQUIRED | The leaf the event applies to |
| `old_version` | `INTEGER(32, signed=false)` | `INT32` | REQUIRED | Pre-event template version |
| `new_version` | `INTEGER(32, signed=false)` | `INT32` | REQUIRED | Post-event template version (equal to `old_version` for the rejection variant) |
| `old_template` | `STRING` (canonical JSON) | `BYTE_ARRAY` | REQUIRED | The token sequence of the pre-event template (matches RFC 0001 §6.4's non-optional `old_template: String`). For `TemplateTypeExpanded` and `TemplateWideningRejectedDegenerate` (variants where the template tokens don't change), `old_template == new_template` |
| `new_template` | `STRING` (canonical JSON) | `BYTE_ARRAY` | REQUIRED | The token sequence of the post-event template (matches RFC 0001 §6.4's non-optional `new_template: String`). Always set: `TemplateWidened` carries the post-widen template; `TemplateTypeExpanded` and `TemplateWideningRejectedDegenerate` carry the unchanged template (equal to `old_template`) |
| `positions_widened` | `LIST<INT32>` | as schema | REQUIRED | Always written; the list is empty for `TemplateTypeExpanded` (no positions involved) and `TemplateWideningRejectedDegenerate` (the would-be widening was rejected). For `TemplateWidened`, the positions that gained `<*>`. Mirrors RFC 0001 §6.4 `positions_widened: Vec<u16>` |
| `slots_expanded` | `LIST<STRUCT<slot_index: INT32, types_added: LIST<INT32>>>` | as schema | REQUIRED | Always written; the list is empty for `TemplateWidened` and `TemplateWideningRejectedDegenerate`. For `TemplateTypeExpanded`, one element per slot whose type set grew, each carrying the wildcard-slot ordinal plus the `ParamType` ordinals added (RFC 0001 §6.4 `slots_expanded: Vec<SlotExpansion>`; `SlotExpansion = { slot_index, types_added }`) |
| `triggering_line_hash` | (no logical type) | `FIXED_LEN_BYTE_ARRAY(16)` | REQUIRED | Blake3 hash of the raw triggering line `L_raw` (RFC 0001 §6.4 `triggering_line_hash: [u8; 16]`); enables cross-referencing the audit event with the data record that caused it |
| `triggering_line_sample` | `STRING` | `BYTE_ARRAY` | OPTIONAL | First 256 bytes of `L_raw`, UTF-8 lossy-decoded if necessary (RFC 0001 §6.4 `triggering_line_sample: Option<String>`); `NULL` when the sample was redacted for retention policy |
| `reason` | `STRING` | `BYTE_ARRAY` | OPTIONAL | `NULL` for variants other than `TemplateWideningRejectedDegenerate`; the degenerate-template guard's diagnostic string otherwise |

The canonical-JSON encoding of `old_template` / `new_template`
is `["lit0", "<NUM>", "lit2", ...]` — the same shape the miner's
in-memory `Vec<OwnedToken>` produces.

**Audit encoding policy** (parallel to §3.6's data-file table;
the audit stream is low-volume so page indexes and bloom filters
are unnecessary defaults, but the policy needs to be explicit
under §3.1's "RFC pins per-column encoding policy" commitment):

| Column | Dictionary | Page index | Bloom filter | Rationale |
|---|---|---|---|---|
| `tenant_id` | yes | no | no | Bounded per cluster |
| `timestamp` | no | yes | no | `DELTA_BINARY_PACKED` Parquet encoding plus ZSTD compression (same shape as data-file `time_unix_nano`); page index supports time-range pruning on drift queries |
| `event_kind` | yes | yes | no | Three values today, plus future ordinals |
| `event_type` | yes | yes | no | Same bounded set as `event_kind`; predicate-pushdown surface for the RFC 0001 §6.7 drift query |
| `template_id` | yes | yes | no | Bounded by tenant template count; bloom filter is unnecessary at audit volume |
| `old_version`, `new_version` | yes | no | no | Small per template |
| `old_template`, `new_template` | no | no | no | Per-tenant repetitive but variable-length JSON; defer the dict decision until bench data exists |
| `positions_widened` (list values) | yes | no | no | Small INT32s |
| `slots_expanded` (list / struct values) | yes | no | no | Same |
| `triggering_line_hash` | no | no | no | Near-random 16 bytes, dict loses |
| `triggering_line_sample` | no | no | no | High-entropy text, dict loses |
| `reason` | yes | no | no | A small set of guard diagnostic strings in practice |

Compression codec follows §3.5 (`ZSTD-3` across every column).
Anything not in the table above takes the writer's defaults; the
table covers every row-level column declared in §3.7.

Audit files are flushed independently of data files: a single
write to the cluster's audit sink does not force a data flush,
and vice versa. The writer guarantees no audit event is lost
across crashes by routing audit events through the same WAL
path as data records (a contract that lands with the post-MVP
`ourios-wal` crate; until then audit-event durability is
in-memory and the corpus bench accepts that).

### 3.8 Schema-evolution policy

The §3.5 invariant from `CLAUDE.md` is normative: "All schema
changes go through the schema RFC process." RFC 0005 establishes
the **baseline** schema; subsequent changes follow these rules:

1. **Adding a column.** Always `OPTIONAL`. An amendment to this
   RFC names the column, its type, its default behaviour for
   readers that haven't been upgraded, and its source/derivation.
   No data-migration is required — old files lack the column,
   readers surface `None` (or the documented default), new files
   include it.
2. **Renaming a column.** Forbidden in-place. The path is: add
   the new name as a new optional column, dual-write for one
   release, deprecate the old name in a later RFC, drop the old
   name in the release after that.
3. **Changing a column's type.** Forbidden in-place. Add a new
   column (`<name>_v2` or a semantically meaningful new name),
   migrate, drop. The amendment RFC pins the migration plan.
4. **Removing a column.** Requires an RFC against `CLAUDE.md`
   §3.5. The migration plan accompanies the RFC: either every
   historical file is rewritten, or queries against the removed
   column become a documented error.
5. **Changing a column's encoding policy** (e.g. enabling
   dictionary on `body`, dropping a bloom filter). Permitted in
   an RFC patch — encoding is not part of the *logical* schema,
   so readers don't break, but a benchmark must show the change
   doesn't regress A1/B1/B2.

The PR description that touches the schema must explicitly call
out which rule above applies, mirroring the RFC 0001 §6.7 convention
for invariant-touching PRs.

### 3.9 Reader contract

The reader has three normative requirements:

1. **Unknown columns are silently ignored.** A file produced by
   a future writer that adds columns the current reader doesn't
   know about must read successfully; the unknown columns are
   dropped on the floor. This is what makes amendment-by-addition
   (§3.8 rule 1) cheap.
2. **Missing columns surface as documented defaults.** A file
   produced by an earlier writer that lacks columns the current
   reader expects must read successfully; the missing columns
   default to:
   - OPTIONAL columns → `None`. Per §3.8 rule 1, every
     amendment-added column is OPTIONAL, so this bullet covers
     the entire amendment-additive surface — there is no
     "REQUIRED-added-in-amendment" case to default.
   - The baseline REQUIRED columns from this RFC — the reader
     errors. A file missing baseline columns is corrupted or
     written by an incompatible writer; falling through to a
     made-up default would corrupt downstream query results.
3. **Row-vs-path partition validation.** For every row read
   under a partition-aware path (i.e. via `Reader::open_partition`
   or the DataFusion `ListingTable` integration that feeds a
   partition tuple in), the reader compares the row-level
   `tenant_id` against the partition path's `tenant_id` segment
   and the row-level `time_unix_nano`'s UTC year / month / day
   / hour against the path's time-bucket segments. Mismatch is
   a **hard read error** that names the offending row and the
   partition path. The row value is authoritative (the talk and
   RFC 0001 §6.1's row-as-source-of-truth rule); the path is the
   partition-pruning index. A diagnostic `Reader::open_file`
   helper that opens a single file without a partition tuple
   skips this validation and surfaces records as-stored — that
   mode is not exposed through the production query path.

Unknown `ParamType` ordinals (i.e. a value the reader doesn't
know about) are surfaced as `ParamType::Unknown` — a reserved
catch-all variant. Queries against records carrying unknown
variants pass through to the application layer to decide what
to do (the RFC 0001 §6.6 reconstruction path treats unknown
variants as lossy and falls back to the body column, which is
why RFC 0001 §6.5's overflow-forces-body-retention rule is
paired with this).

### 3.10 Crate shape

`crates/ourios-parquet/` per the §7 target layout in
`CLAUDE.md`. The public surface is intentionally small:

- `Schema` — a singleton describing the data-file schema; one
  function per amendment that gates an additive column.
- `AuditSchema` — the parallel singleton for the audit stream.
- `Writer` — opens a file at a partition path, appends rows in
  the §3.2 column order, rotates row groups at the §3.5
  threshold.
- `Reader` — opens a file (or a directory of files; partition
  discovery is part of the reader's job), surfaces records as
  `MinedRecord`s with the §3.9 contract.
- `AuditWriter` / `AuditReader` — same shapes for the audit
  series.

No trait abstraction over `Writer` or `Reader` until a second
implementation is named in an RFC. Pre-abstracting when only
one consumer exists picks an axis for the trait before the
shape of the second consumer is visible, and an extracted
trait that turns out to fit only one consumer is harder to
re-shape than the concrete type would have been. Phase 3's
DataFusion table provider is one
consumer of `Reader`; the bench is another; both are concrete,
neither demands a trait.

## 4. Alternatives considered

### 4.1 Apache Iceberg or Delta Lake on top of Parquet

A table-format layer (Iceberg, Delta) would give us schema
evolution, snapshots, and time-travel queries for free.
Rejected for MVP: both pull in a large dependency surface
(metastore plumbing, transaction logs, manifest files) for
features (snapshots, time-travel) the thesis gates don't need.
A future RFC can adopt Iceberg as a layer over the Parquet
files defined here — Iceberg is additive on top of Parquet, so
the §3.2 schema doesn't need to change. Adopting it now would
multiply the dependency footprint without moving the thesis.

### 4.2 Apache Arrow IPC files instead of Parquet

Arrow IPC is faster to read into Arrow memory but lacks
Parquet's row-group pruning, page index, and bloom filters —
the exact features Pillar 1 of `CLAUDE.md` §2 names as
load-bearing for thesis-gate B1. Rejected for the same reason
Parquet was chosen in the first place.

### 4.3 Typed STRUCT encoding of `AnyValue`

Encode the OTLP `AnyValue` discriminated union as a recursive
Parquet STRUCT, with one optional field per variant and explicit
recursion-depth unrolling for `array` / `kvlist`. Rejected for
MVP: Parquet's flat-nested model doesn't support true
recursion; any encoding caps recursion depth at the schema
declaration, which is a hard limit operators can't override
without a schema change. Canonical JSON in a BYTE_ARRAY is
unambiguously faithful and defers the typed-attribute query
story to a future RFC with a named consumer.

### 4.4 One concatenated file series (data + audit)

Carry audit-event rows in the data file with a discriminator
column. Rejected: audit volume is orders of magnitude smaller
than data volume; co-locating them defeats partition pruning
for both ("give me all widening events" would have to scan the
data partition, "give me all log records at time T" would scan
through audit rows). The two-file-series shape is the natural
operational separation.

### 4.5 Compaction in MVP

Background compaction (small-file consolidation) was considered
for Phase 2. Rejected: `docs/roadmap.md` §4 Phase 2 explicitly
parks it post-MVP, on the rationale that corpus runs are bounded
and a single Parquet file per phase is acceptable. Production
deployments accumulating sustained traffic will need compaction
before the H4 file-size detection threshold fires; that's a
post-MVP RFC.

### 4.6 Apache Avro for the audit-event stream

Avro is a natural fit for sparse event streams. Rejected:
Pillar 1 commits the project to Parquet end-to-end; running two
file formats in one bucket doubles the operational surface
(reader libraries, schema-registry-shape, partition-discovery
code) for the marginal benefit of slightly better encoding of a
column the bench won't measure.

## 5. Acceptance criteria

> **Scenario RFC0005.1 — Round-trip preserves every §3.2 row-level column**
> - **Given** a `MinedRecord` populated with every row-level column
>   in §3.2 (every OPTIONAL field set to `Some`, every variant of
>   `body_kind` exercised across a batch — including the row-level
>   `tenant_id`)
> - **When** the batch is written to a Parquet file by the writer
>   and read back by the reader via `Reader::open_partition` (the
>   production query path)
> - **Then** for every column whose Rust type in `MinedRecord` is
>   a raw byte container (`trace_id: Option<[u8; 16]>`,
>   `span_id: Option<[u8; 8]>`, `body: Option<Bytes>`), the
>   recovered bytes equal the original bytes byte-for-byte
> - **And** for every typed column (integers, floats, booleans,
>   timestamps, enum ordinals, plain strings, the `params` and
>   `separators` lists), the recovered value equals the original
>   under the column's Rust-level equality — UTF-8 equality for
>   `String`, numeric equality for integers/floats/timestamps,
>   element-wise equality for `Vec<T>`
> - **And** for the canonical-encoded structural columns
>   (`attributes: Vec<KeyValue>` and `resource_attributes:
>   Vec<KeyValue>` — encoded as canonical JSON `BYTE_ARRAY` on
>   disk per §3.3), the recovered `Vec<KeyValue>` equals the
>   original under structural equality (the canonical encoding
>   is bidirectional and deterministic per RFC 0001 §6.1, so
>   structural equality is the testable property at the
>   `MinedRecord` boundary; byte equality on the encoded bytes
>   follows as a corollary but is not the primary assertion)
> - **And** the round-trip equality assertion does **not** include
>   the pure-partition pseudo-columns (`year`, `month`, `day`,
>   `hour`); those are covered by RFC0005.5 (partition layout) and
>   RFC0005.11 (row-vs-path validation)

> **Scenario RFC0005.2 — Missing column tolerance (old-file reader path)**
> - **Given** a Parquet file produced by a hand-rolled writer that
>   omits an OPTIONAL column the current schema declares
> - **When** the current reader reads the file
> - **Then** records surface with `None` for the absent column
> - **And** no error is raised

> **Scenario RFC0005.3 — Unknown column tolerance (forward compatibility)**
> - **Given** a Parquet file produced by a hand-rolled writer that
>   includes a column the current reader's schema does not declare
> - **When** the current reader reads the file
> - **Then** the unknown column is silently ignored
> - **And** every declared column reads through correctly
> - **And** no error is raised

> **Scenario RFC0005.4 — Baseline REQUIRED column missing → reader errors**
> - **Given** a Parquet file produced by a hand-rolled writer that
>   omits one of the §3.2 baseline REQUIRED columns
> - **When** the current reader attempts to read it
> - **Then** the reader returns an error naming the missing column
> - **And** no records are surfaced

> **Scenario RFC0005.5 — Partition layout follows §3.4**
> - **Given** a record stream spanning two tenants, three hours, and
>   one of the records carries a tenant id with non-ASCII characters
> - **When** the writer flushes records to the bucket
> - **Then** files are placed under
>   `data/tenant_id=<urlencoded_id>/year=YYYY/month=MM/day=DD/hour=HH/<uuidv7>.parquet`
> - **And** every record inside a file shares the partition tuple

> **Scenario RFC0005.6 — Row-group size lands inside H4 target**
> - **Given** a corpus run producing more than 256 MiB of mined
>   records under the production writer (not the corpus-mode
>   single-file path)
> - **When** the writer flushes Parquet files
> - **Then** every emitted row group's `total_uncompressed_size`
>   (Parquet column-chunk metadata, sum across columns) is at
>   least 128 MiB and at most 1 GiB
> - **Except** the final row group of a file, which may be smaller

> **Scenario RFC0005.7 — Audit-event stream is a separate file series**
> - **Given** a corpus run that triggers at least one RFC 0001
>   §6.4 `event_type = template_widened` event (the Rust variant
>   is `TemplateWidened`)
> - **When** the cluster's audit sink flushes
> - **Then** audit events land under `audit/tenant_id=<id>/...`, not
>   interleaved with the data file series
> - **And** the emitted audit record is populated for every row-
>   level column declared in §3.7's audit-schema table, with NULL
>   appearing only on the explicitly-OPTIONAL columns documented
>   for the variant (e.g. `reason` is NULL for `template_widened`;
>   `slots_expanded` is an empty list)

> **Scenario RFC0005.8 — `body` column carries no dictionary encoding**
> - **Given** a corpus run that retains at least 100 unique high-
>   entropy body strings (e.g. via RFC 0001 §6.3 lossy-zone or
>   RFC 0001 §6.5 overflow)
> - **When** the writer flushes the Parquet file
> - **Then** the `body` column chunk's `compression` codec is
>   `ZSTD` (Parquet `CompressionCodec` field)
> - **And** the `body` column chunk's `encodings` list does NOT
>   include `PLAIN_DICTIONARY` or `RLE_DICTIONARY` (Parquet
>   `Encoding` enum)
> - **And** the `body` column chunk's `dictionary_page_offset`
>   is unset (`None`) in the column-chunk metadata — there is
>   no dictionary page on disk for this column

> **Scenario RFC0005.9 — Unknown `ParamType` ordinal surfaces as `Unknown`**
> - **Given** a Parquet file with a `params.type_tag` value that the
>   current reader's `ParamType` enum doesn't recognise (e.g. ordinal
>   `99`)
> - **When** the reader reads it
> - **Then** the resulting `Param.type_tag` is `ParamType::Unknown`
> - **And** the record's `reconstruct` call surfaces it as lossy
>   (consistent with RFC 0001 §6.6's fallback path)

> **Scenario RFC0005.10 — Schema declaration is greppable and immutable**
> - **Given** the `Schema` singleton defined in `ourios-parquet`
> - **When** the test suite extracts the column list from `Schema`
>   and compares it against the column list pinned in this RFC
> - **Then** the two lists are equal in name, type, and repetition,
>   in declared order

> **Scenario RFC0005.11 — Row-vs-path validation on partition mismatch**
> - **Given** a Parquet file whose row-level `tenant_id` (or UTC
>   year / month / day / hour from `time_unix_nano`) disagrees with
>   the partition-path segments the file lives under
> - **When** the reader opens the file via `Reader::open_partition`
> - **Then** the reader returns a hard error naming the offending
>   row, the row's value, and the partition path's value
> - **And** no records are surfaced from the file

## 6. Testing strategy

- **RFC0005.1** — property test in
  `crates/ourios-parquet/tests/roundtrip.rs` using `proptest` to
  generate `MinedRecord`s spanning every column variant; asserts
  byte-equality after a round trip through the writer and reader.
  Corpus integration test in the same file drives the H7.1
  corpus through writer → reader and asserts the same property
  end-to-end.
- **RFC0005.2, RFC0005.3, RFC0005.4** — schema-evolution tests
  in `crates/ourios-parquet/tests/evolution.rs`. Each test
  builds a Parquet file with the `parquet` crate directly (not
  through the project's writer), exercising a specific shape:
  missing-OPTIONAL, unknown-column, missing-REQUIRED. Asserts
  the §3.9 reader contract.
- **RFC0005.5** — integration test in
  `crates/ourios-parquet/tests/partition.rs` that drives the
  writer with a synthetic multi-tenant, multi-hour stream and
  asserts the bucket layout via filesystem inspection. The
  non-ASCII tenant id case is a sub-test.
- **RFC0005.6** — corpus integration test in
  `crates/ourios-parquet/tests/sizing.rs`. Generates ≥256 MiB of
  records, flushes through the writer, parses each emitted file's
  Parquet footer, asserts row-group sizes inside the H4 range.
  Marked `#[ignore]` by default (slow); contributors run it
  manually via `cargo test --ignored`. Scheduling it on a CI
  cadence is an open question (§7) — the project's CI workflow
  has no `schedule` trigger today, so the RFC does not commit to
  one.
- **RFC0005.7** — integration test in
  `crates/ourios-parquet/tests/audit.rs` that wires the audit
  sink to the writer's audit path, triggers a widening through
  the miner, flushes, and reads back the audit file. Asserts
  the §3.7 column set.
- **RFC0005.8** — Parquet-metadata inspection test in
  `crates/ourios-parquet/tests/encoding.rs`. Drives 100+ unique
  bodies through the writer, opens the resulting file's footer
  via the `parquet` crate's column-chunk metadata, asserts the
  `body` column's `compression` is `ZSTD` and its `encodings`
  list does not include `PLAIN_DICTIONARY` or `RLE_DICTIONARY`
  (the two distinct Parquet-metadata fields per RFC0005.8).
- **RFC0005.9** — unit test in `crates/ourios-parquet/src/reader.rs`
  with an in-memory Parquet file built directly from `arrow`
  arrays carrying a forged `99` in the `type_tag` list.
- **RFC0005.10** — unit test in
  `crates/ourios-parquet/tests/schema_pin.rs` that holds a const
  expected-column-list and compares against `Schema::columns()`.
  This is the "schema-as-spec" pin: adding a column to `Schema`
  without updating the expected list (and, by implication, this
  RFC) fails the test, mirroring the RFC0004.3 pattern.
- **RFC0005.11** — integration test in
  `crates/ourios-parquet/tests/partition_validation.rs` that
  builds Parquet files at deliberately mismatched partition
  paths (row says `tenant_id = a`, path segment says
  `tenant_id=b`) and asserts the reader's hard-error path fires
  with the documented diagnostic. Sub-tests cover the four
  time-bucket parts (`year`/`month`/`day`/`hour`).

Criterion benchmarks (in `ourios-bench`, Phase 3 territory) will
measure A1 (compression ratio) and B1/B2 (predicate-pushdown
latency) against the schema this RFC specifies; those numbers
are normative for the maturity-stage move from `green` to
`validated`.

## 7. Open questions

- [ ] **Compression codec.** ZSTD-3 is the default per §3.5;
  ZSTD-22 trades CPU for ratio. The A1 measurement decides
  whether to add `zstd_level` as a tunable per RFC 0004. Defer
  until A1 numbers exist.
- [ ] **Bloom filter sizing.** §3.6 names `template_id` as the
  one column with a bloom filter; the false-positive rate is a
  Parquet writer parameter (Arrow default is 1%). Lower FPR
  trades file size for query selectivity. Defer until B2
  numbers exist.
- [ ] **Audit-event retention.** Audit events have a different
  retention policy than log records (audits should outlive the
  data they audit, for forensics). The retention plumbing is
  post-MVP (no compaction = no expiry in MVP); the RFC notes
  the asymmetry but does not pin a policy.
- [ ] **Partition-discovery API on the reader.** The reader has
  to enumerate files under a `<bucket>/data/` prefix and decode
  the Hive partition values to apply predicate-pushdown.
  Whether this is in-crate (`Reader::open_partition`) or
  delegated to DataFusion's `ListingTable` is a Phase 3 wiring
  decision; for the standalone reader tests the bench will use
  whichever is simplest.
- [ ] **Concurrent writers per partition.** Two writers writing
  to the same `tenant_id=…/hour=HH/` simultaneously is fine
  (UUIDv7 prevents filename collision), but readers that
  enumerate partitions during an active write may see partial
  files. The reader contract assumes a file is either complete
  or absent. The atomic-publish convention (write to a temp
  path, rename on close) is the writer's responsibility; the
  reader does not need to do anything special. Defer the writer
  PR to nail this down.
- [ ] **Scheduled CI cadence for the slow tests.** RFC0005.6
  (row-group sizing) and any future criterion benchmarks are
  marked `#[ignore]` and rely on `cargo test --ignored` /
  manual invocation. Adding a GitHub Actions `schedule:` trigger
  (e.g. nightly at 03:00 UTC) so these run automatically is a
  follow-up workflow PR, not part of this RFC. The RFC notes
  the gap; the workflow PR will land alongside the Phase 3
  `ourios-bench` benchmark implementation (`docs/roadmap.md`
  §4 Phase 3).

## 8. References

- `CLAUDE.md` §1 (project charter), §2 (architectural pillars —
  Parquet, template miner, DataFusion), §3.2 (no unbounded
  cardinality in `params`), §3.5 (Parquet schema changes
  require a migration plan), §3.6 (object storage is the source
  of truth), §3.7 (multi-tenancy from day one), §5.1 (RFC
  process), §7 (target repository layout — `ourios-parquet` is
  the named crate).
- RFC 0001 §6.1 (`MinedRecord` data model, OTLP-derived
  columns, body representation including the OTLP-canonical
  JSON encoding rule), §6.4 (widening events that this RFC's
  audit-event stream carries), §6.5 (`OVERFLOW` marker + forced
  body retention — the source of unbounded values in the `body`
  column), §6.6 (reconstruction — the consumer of the schema's
  `params` / `separators` / `lossy_flag` columns), §6.7
  (template versioning), §9 (cross-RFC contracts pending —
  audit-event Parquet stream).
- RFC 0002 (query DSL, drafted) — Phase 3 consumer of the
  reader.
- RFC 0003 (OTLP receiver, drafted) — Phase 3 producer of
  records that feed this schema.
- RFC 0004 (configuration policy) §3 (tunables-vs-invariants —
  this RFC's encoding policy choices are *not* tunables; they
  are RFC-amendment territory).
- `docs/hazards.md` H1 (silent template merges — audit-event
  stream is the operational signal), H4 (small-file problem —
  the row-group and file-size targets in §3.5), H5 (template
  schema evolution — the schema-evolution rules in §3.8).
- `docs/benchmarks.md` A1 (compression ratio — gated on this
  RFC's encoding policy), B1 (predicate-pushdown latency —
  gated on this RFC's page index / partition layout), B2
  (template-exact query latency — gated on this RFC's bloom
  filter on `template_id`).
- `docs/roadmap.md` §4 Phase 2 (the capability set this RFC
  opens), §5 (deliberately out of MVP — compaction, the
  post-MVP follow-up RFC named here).
- Apache Parquet Format specification (file format, page
  index, bloom filter, `LIST` encoding) — project site
  <https://parquet.apache.org/>; the normative format spec
  lives in the repository at
  <https://github.com/apache/parquet-format>.
- OpenTelemetry Logs Data Model — `AnyValue`, normative
  source at
  <https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/logs/data-model.md>.
- OpenTelemetry Protocol (OTLP) specification — the JSON
  binding referenced by the canonical-encoding rule for
  `body_kind = Structured` lives at
  <https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md>
  (see the "OTLP/HTTP" section's proto3-JSON mapping).
