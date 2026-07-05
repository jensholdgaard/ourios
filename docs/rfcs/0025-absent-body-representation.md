---
rfc: 0025
title: Absent-body representation and permanent-encode-error quarantine (RFC 0005 amendment)
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-05
supersedes: —
superseded-by: —
---

# RFC 0025 — Absent-body representation and permanent-encode-error quarantine (RFC 0005 amendment)

## 1. Summary

A **legal** OTLP log record with an absent body (`LogRecord.body`
unset) is currently a poison pill: the receiver materializes it
faithfully (`body: None`), the miner emits it faithfully
(`BodyKind::Absent`, RFC 0001 §6.1), and the Parquet encode rejects
it **permanently** (`BatchError::UnsupportedAbsentBody` — RFC 0005
§3.2's `body_kind` column pins ordinals `0 = String,
1 = Structured`). The ingest sink retains its buffer on flush error,
so one absent-body record halts Parquet persistence for its
`(tenant, hour)` partition forever and pins buffer memory (#362,
found by the RFC 0024 adversarial suite on its first run).

This RFC amends RFC 0005 with:

1. **A third `body_kind` ordinal** — `2 = Absent` — with a `NULL`
   `body` cell, making the wire-legal state representable on disk.
2. **A read-path contract** — absent-body rows render with no body
   (the RFC 0017 `LogRow` carries none), never as an empty string.
3. **A sink quarantine rule** — a *permanent* encode error must
   never wedge a partition: the sink separates the rejected
   record(s) from the buffer, persists the rest, and surfaces the
   rejection through the existing flush-error counter with an
   `error.type` attribute. Defense in depth: with (1) in place,
   `UnsupportedAbsentBody` disappears, but the wedge mechanism would
   fire identically for any future permanent `BatchError`
   (timestamp overflow is one that exists today).

## 2. Motivation

- **Absent bodies are spec-legal and real.** OTLP permits records
  with no body — event-shaped records carrying only `event_name` +
  attributes are the canonical case. A backend that wedges on them
  fails the RFC 0003 fidelity posture from the wire side.
- **The failure mode is silent and unbounded.** The WAL holds the
  acknowledged data (§3.4 holds), but the ingest→Parquet path stalls
  for the partition; buffers grow to the memory ceiling; nothing
  reaches object storage. Operators see a flush-error counter tick
  and stalled data — the worst diagnosis surface.
- **Timestamp overflow shares the mechanism.** A record whose
  `observed_time_unix_nano` exceeds `i64::MAX` is also a permanent
  encode rejection today; quarantine fixes both.

## 3. Design

### 3.1 Schema (RFC 0005 §3.2 amendment)

`body_kind` gains ordinal `2 = Absent`. For such rows the `body`
column is `NULL`, `params` and `separators` are empty, and
`lossy_flag = true` is **retired for this case**: absence is not
loss — the row reconstructs to "no body" exactly. The miner's
emission changes from `lossy_flag = true` to `false` for
`BodyKind::Absent` rows (RFC 0001 §6.1 note: reconstruction is
*defined* and total — it renders nothing).

**Migration (§3.5 compliance):** additive only. Old files never
contain ordinal 2 and remain fully readable. Old *readers* (any
pre-amendment binary) encountering a future file with ordinal 2 must
error per the §3.2 shape-validation contract — this is the standard
forward-compatibility posture already pinned by RFC0005.14
(unknown-ordinal rejection), and operators upgrade readers before
writers as with every schema-affecting release. No historical
rewrite.

### 3.2 Read path (RFC 0017 amendment)

- `Reader` accepts ordinal 2 and materializes
  `body_kind = Absent`, `body = None`.
- Query rendering (`LogRow`): the body field is absent (`None` /
  omitted in JSON), **not** `""` — an empty string body is a
  different legal record.
- The RFC 0002 DSL: absent-body rows match non-body predicates
  normally; body-text predicates never match them.

### 3.3 Sink quarantine (ourios-ingester)

On flush, when the encode fails with a **permanent** `BatchError`
(the existing `is_transient` split already classifies this):

1. Bisect the buffer to the offending record(s) (binary search on
   singleton encodes — O(k·log n) for k poison records, and k is
   almost always 1).
2. Emit the poisoned record(s) to the audit stream (event kind:
   `record_quarantined`, carrying tenant, partition, the error text,
   and the record's WAL position) and drop them from the buffer.
3. Flush the remainder normally.
4. Count via the existing flush-error counter with `error.type` =
   the `BatchError` variant name (per the OTel recording-errors
   convention — no new metric).

The WAL retains the record (durability unchanged); the quarantine
audit event is the operator's pointer for manual recovery or replay
after a fix. No new config: quarantine is not optional behavior —
the alternative is the wedge.

## 4. Alternatives considered

- **Map absent to `Body::String("")` at the receiver.** Destroys
  fidelity (RFC 0017/0018): empty-string and absent are distinct
  wire states, and the read path already distinguishes them.
- **Drop absent-body records at the receiver.** Data loss for
  spec-legal input; violates the acknowledged-data contract.
- **Retry-forever with alerting (status quo + alarm).** Leaves the
  partition wedged and the memory pinned; alerting on an unbounded
  failure is not a fix.
- **Quarantine to a side file instead of the audit stream.** A new
  on-disk artifact class (lifecycle, retention, discovery) for a
  rare event the audit stream already models.

## 5. Acceptance criteria

Scenario ids `RFC0025.<m>`.

> **Scenario RFC0025.1 — absent bodies round-trip.** Given a mined
> `BodyKind::Absent` record, When it is written and read back, Then
> every RFC 0005 §3.2 column round-trips, `body` is `NULL`, and the RFC 0024
> P1 suite's pinned-rejection arm for absent bodies is **replaced**
> by round-trip assertion.

> **Scenario RFC0025.2 — old files unaffected.** Given a
> pre-amendment file, When read by the amended reader, Then results
> are identical to the prior reader (committed-fixture parity, the
> RFC 0021 §6 discipline).

> **Scenario RFC0025.3 — rendering distinguishes absent from
> empty.** Given one row with `body = ""` and one with
> `body_kind = Absent`, When both are rendered through the query
> path, Then the empty-string row carries `""` and the absent row
> carries no body field.

> **Scenario RFC0025.4 — the sink no longer wedges.** Given a
> buffer containing an absent-body record (pre-amendment encoder
> simulated) or a timestamp-overflow record, When flush runs, Then
> the healthy records persist, the poisoned record is quarantined to
> the audit stream with a `record_quarantined` event, and subsequent
> flushes of the partition succeed.

> **Scenario RFC0025.5 — quarantine telemetry.** Given a quarantine,
> Then the existing flush-error counter increments with
> `error.type` set to the `BatchError` variant, and no new metric
> name is introduced.

## 6. Testing strategy

RFC0025.1/.3 as integration tests in `ourios-parquet` /
`ourios-querier`; RFC0025.2 via the committed pre-amendment fixture;
RFC0025.4/.5 in `ourios-ingester` (the quarantine path is
deterministic — no property machinery needed, though the RFC 0024
adversarial umbrella inherits coverage automatically once the P1 arm
flips).

## 7. Open questions

1. **Miner sentinel for absent bodies.** Absent rows currently take
   the `NO_TEMPLATE` id with `lossy_flag = true`; with §3.1 they
   keep `NO_TEMPLATE` but drop the lossy flag. Should they instead
   share the structured-sentinel mechanism (per
   `(severity, scope)`)? Deferred — `NO_TEMPLATE` is adequate and
   queryable.
2. **Quarantine replay tooling.** The audit event carries the WAL
   position; an operator `replay-quarantined` subcommand is deferred
   until demand exists.

## 8. References

- #362 (the finding), RFC 0024 §2 (the suite that found it),
  RFC 0005 §3.2 (`body_kind` ordinals), RFC 0001 §6.1
  (`BodyKind::Absent` emission), RFC 0017 (read-path fidelity),
  RFC 0008 (WAL durability the quarantine leans on), RFC 0015 §9 of
  RFC 0008 (audit-event precedent for system-scoped events).
