---
rfc: 0043
title: Derive event_name from the legacy event.name attribute at ingest
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-29
supersedes: —
superseded-by: —
---

# RFC 0043 — Derive `event_name` from the legacy `event.name` attribute at ingest

> **Status: `green` (2026-07-29).** All seven §5 criteria pass: the
> derivation + boundary shapes landed in #668 (RFC0043.1–.4/.7, both
> encodings through the one `materialize_record` seam), and #669 closed
> .5 (attr-only records through the real miner into a real store,
> matched end-to-end by `event_name ==`) and .6 (the id-separation
> observable, criterion refined inline — the originally referenced
> counter does not exist as a distinct instrument). No thesis-gate
> applies; `accepted` is a maintainer flip.
>
> *(`specified`, same date: §5 criteria written and testable; the one
> fidelity question (§7) resolved in the design — the attribute is
> preserved verbatim, the field is* derived*, nothing is corrected.)*

## 1. Summary

When an incoming `LogRecord` has no top-level `event_name` but carries an
`event.name` attribute, ingest populates the stored record's `event_name`
from that attribute — and keeps the attribute itself byte-for-byte. This
is the OTel spec's own migration story executed at the backend: the
`event.name` attribute is the legacy precursor of the top-level
`EventName` field, and real sources (Claude Code, opencode-plugin-otel)
still emit only the attribute. With the field populated, RFC 0037's
event-keyed templating engages for those sources and
`event_name == "…"` becomes the idiomatic DSL filter the OTel events
semconv prescribes ("when users query for a specific event name…").

## 2. Motivation

**The idiomatic query exists but the data never reaches it.** The DSL
already exposes `event_name` as a queryable field and the OTLP receiver
already parses the wire field — but our two flagship GenAI sources
predate the field and emit only the `event.name` attribute, so every
such record stores `event_name: NULL`. Users fall back to `body ==`
(bitten by #664) or attribute-implicit grouping.

**The events semconv points here, not at body.** Semconv MUST NOT give
`body` a value beyond a display message, and event identity queries are
`EventName`'s job. This RFC makes the backend meet the spec's intent for
sources that haven't caught up.

**RFC 0037 gets its keying for free.** Event-keyed templating (§3.1)
activates on `event_name` presence; today it never engages for
Claude Code/opencode events. Deriving the field turns their template
handling from body-mining into the designed event-keyed path.

## 3. Proposed design

At the OTLP decode boundary (`ourios-core` OTLP conversion, both
protobuf and JSON paths per the RFC0003.6 checklist):

1. If `LogRecord.event_name` is set **and non-empty**, it wins. The
   `event.name` attribute, if also present, is stored untouched — no
   comparison, no correction, no flag (a mismatch between the two is
   source telemetry, and we preserve it; the read path returns both as
   received).
2. If `LogRecord.event_name` is unset **or empty** and an `event.name`
   attribute is present with a **non-empty string** value, the stored
   record's `event_name` is set to that string. The attribute remains
   in `attributes` verbatim — derivation, not a move. Non-string and
   empty-string `event.name` values derive nothing.
3. Neither present → `event_name` stays NULL, exactly as today.

"Set" is defined identically for both encodings: protobuf cannot
distinguish an absent string field from an empty one (proto3 default),
and RFC0003.6 JSON may spell absence as a missing key, `null`, or `""`
— all three read as unset. An empty string is therefore never a value,
on either side of the derivation, and the two decode paths cannot
diverge (RFC0043.3/.7).

The invariant posture: the OTLP-fidelity rule (preserve / flag / never
correct) is untouched because nothing received is altered or dropped —
the derived field is additive, and a reader comparing stored attributes
against the source's export sees byte identity. Downstream (mining,
RFC 0037 keying, the DSL field, the RFC 0032 query-schema document) all
consume `event_name` unchanged; they simply see it populated for more
sources.

## 4. Alternatives considered

- **Do nothing; teach `body ==` instead.** #664's fix (RFC 0044) makes
  `body ==` correct, but the semconv is explicit that event identity is
  the event name's job; leaving the field NULL keeps the idiomatic
  query dead for the most important corpus and keeps RFC 0037's keying
  inert.
- **Move the attribute into the field (hoist-and-drop).** Violates the
  fidelity rule — the stored attributes would no longer match what the
  source exported.
- **Collector-side remapping (OTTL).** Works per deployment, but every
  deployment must know to do it; the backend doing the spec's documented
  migration once is strictly less operational surface. A deployment that
  remaps anyway hits rule 1 and nothing double-applies.

## 5. Acceptance criteria

- **RFC0043.1 — the wire field wins**
  - **Given** a record with `event_name` set and a differing
    `event.name` attribute
  - **When** it is ingested and read back
  - **Then** the stored `event_name` is the wire field's value
  - **And** the attribute is returned verbatim, unflagged.
- **RFC0043.2 — derivation from the attribute**
  - **Given** a record with no `event_name` and an `event.name` string
    attribute
  - **When** it is ingested and read back
  - **Then** `event_name` equals the attribute's value
  - **And** the `event.name` attribute is still present, byte-identical.
- **RFC0043.3 — both encodings**
  - **Given** the RFC0043.2 record encoded as OTLP protobuf and as
    RFC0003.6 JSON
  - **When** each is ingested
  - **Then** both derive identically.
- **RFC0043.4 — non-string derives nothing**
  - **Given** an `event.name` attribute whose value is not a string
  - **When** ingested
  - **Then** `event_name` stays NULL and the attribute is preserved.
- **RFC0043.5 — the idiomatic query works end-to-end**
  - **Given** an ingested attr-only corpus (Claude Code-shaped fixture)
  - **When** `event_name == "claude_code.api_request"` runs
  - **Then** exactly the api_request records return.
- **RFC0043.6 — RFC 0037 keying engages, observably**
  - **Given** three attr-only **structured** records: two sharing an
    `event.name` (with differing body content) and one with a distinct
    `event.name` (RFC 0037 §3.1 keys `(severity, scope, event_name)`
    for structured bodies; without derivation all three collapse into
    the one no-event sentinel)
  - **When** mined
  - **Then** the two same-name records carry the **same** `template_id`
    despite differing content
  - **And** the distinct-name record carries a **different**
    `template_id` — the separation is the externally visible proof the
    derived name reached the template key, since the sentinel would
    have merged all three. *(Refined at implementation time: the
    originally referenced "event-keyed counter" does not exist as a
    distinct instrument; the id separation is a strictly stronger
    observable.)*
- **RFC0043.7 — empty is never a value, in either encoding**
  - **Given** records with (a) empty-string wire `event_name` plus an
    `event.name` attribute, (b) an empty-string `event.name` attribute
    only, and (c) JSON `null` `event.name` — each encoded as protobuf
    and as RFC0003.6 JSON where representable
  - **When** ingested
  - **Then** (a) derives from the attribute, (b) and (c) derive
    nothing, and the protobuf and JSON results are identical
    shape-for-shape.

## 6. Testing strategy

Unit tests at the decode boundary for .1–.4 (both encodings, per the
RFC0003.6 checklist — struct round-trip alone is insufficient);
integration through ingest→query for .5; a miner-level assertion for .6
against a Claude Code-shaped fixture. No new metrics: derivation is not
an anomaly (the anomaly-telemetry rule reserves `error.type` on the
existing counters for rejections).

## 7. Open questions

- [x] **Fidelity vs. derivation — resolved in §3:** derive additively,
      never mutate received data. The read path returns both.
- [ ] Should the RFC 0032 query-schema document call out `event_name`
      availability per tenant (it is population-dependent)? Deferred —
      the field is already listed as queryable.

## 8. References

- OTel Logs Data Model — `EventName` field; events semconv (§Body:
  "MUST NOT define a value for body except … display message"; §Event
  name: "when users query for a specific event name…").
- RFC 0003 (OTLP receiver) + the RFC0003.6 JSON checklist.
- RFC 0037 (GenAI/structured-event logs) §3.1 event-keyed templates.
- RFC 0044 (template-aware body equality) — the complementary half:
  this RFC gives event filtering its idiomatic path; 0044 makes the
  compatibility path correct. #664 is closed by 0044, not this RFC.
