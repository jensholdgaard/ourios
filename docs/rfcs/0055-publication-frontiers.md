---
rfc: 0055
title: Publication frontiers and tenant settlement
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-15
supersedes: —
superseded-by: —
---

# RFC 0055 — Publication frontiers and tenant settlement

> **Status note.** `drafted`. Split out of RFC 0053. The reviewed prose
> is the non-ownership half of §3.2 on
> [`#802`](https://github.com/jensholdgaard/ourios/pull/802) @ `30a21f80`
> (`PUBLISHED`, frontiers, settlement, tenant slots, object-key intent,
> sidecar geometry). Extract. Do not grow 0053.

## 1. Summary

Unwind safety (RFC 0054) is an in-process property. Across a restart,
recovery must know which record and audit frames already reached the
store, and a mining panic must not leave a tenant tree between a
widening and its audit event.

This RFC specifies:

- a durable per-tenant publication frontier (`PUBLISHED`)
- settlement that rebuilds a panicked tenant from its last installed
  snapshot
- tenant-slot admission (`max_tenants`) as the open-mode backstop

It does not specify the WAL byte bound (RFC 0053) or the unwind
invariant (RFC 0054).

## 2. Motivation

RFC 0054's "one extra object per panic" claim does not survive a
pre-checkpoint crash: recovery replays frames and can regenerate a
durable audit event. Frontiers are the missing watermark. Settlement
is hazard #5 (template-state recovery) for the in-process panic.

## 3. Proposed design

Extract from the 0053 draft, then stop:

- `PUBLISHED` sidecar: per-tenant `{records, audit}` offsets, one
  durable write, geometry sized from `max_tenants`, never grown in
  place; raise-capacity rebuilds both sidecars through `.new` temps.
- Record replay gated on the record watermark; audit replay gated on
  `max(checkpoint, audit watermark)`.
- Settlement: rebuild from last installed snapshot; replay frames;
  discarded tree's buffered records dropped under the sink lock in the
  same section as the swap.
- Tenant slots: `Reserved` / `Held`; count `Held + Reserved` against
  `max_tenants`; seed `Held` from recovered tenants before admit.
- Object-key intent as specified in the last commits on #802
  (normative in-process; cross-restart claim already dropped).
- Tombstone a removed tenant's dictionary slot.

**Amendments this RFC may make**, each as a one-line pointer, not a
new essay: RFC 0046.11 tenant-id length 256→128 to match RFC 0048
§3.1 (already recorded as a resolved question in 0046).

## 4. Alternatives considered

- Rely on recovery's WAL suppression horizon alone. Rejected: it does
  not see in-process duplicates or durable-but-unmarked audit objects.
- Keep this in RFC 0053. Rejected: it is why 0053 cannot converge.

## 5. Acceptance criteria

Write `Given / When / Then` when extracting. Minimum:

- RFC0055.1 restart does not republish a settled audit group
- RFC0055.2 mixed-geometry sidecar restart comes up at the larger
  geometry with ids unchanged
- RFC0055.3 settlement rebuild does not publish under a `template_id`
  the replay did not allocate
- RFC0055.4 `max_tenants` holds across unwind and restart

## 6. Testing strategy

Sidecar fixtures (including half-resized pair and tombstones) plus a
settlement race with two triggers. Details in the quarry draft §6.

## 7. Open questions

- Idempotent object keys across restart (explicitly dropped on #802;
  reopen here or leave closed).
- Whether settlement blocks the global ingest gate or needs a
  per-tenant gate (RFC 0052). Pick one; do not claim both.

## 8. References

- RFC 0005, 0046, 0048, 0052, 0053, 0054
- Source quarry: #802 @ `30a21f80`
