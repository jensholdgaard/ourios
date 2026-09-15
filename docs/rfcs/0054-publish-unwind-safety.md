---
rfc: 0054
title: Publish unwind safety
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-15
supersedes: —
superseded-by: —
---

# RFC 0054 — Publish unwind safety

> **Status note.** `drafted`. Split out of RFC 0053 at the maintainer's
> direction. The reviewed prose lives in
> [`#802`](https://github.com/jensholdgaard/ourios/pull/802) @ `30a21f80`,
> §2.2 and the ownership / drain / worker / publisher parts of §3.2.
> Extract that wording. Do not invent a new protocol.
>
> Depends on RFC 0052 (barrier / sweep) and RFC 0053 (the node still
> accepts or refuses; this RFC does not decide the bound).

## 1. Summary

A panic inside publish can drop batches the age sweep has already taken
out of the sink. The snapshot barrier then reads empty buffers as
"fully drained" and can stamp a WAL mark over frames that never reached
the store (#796). Today the sweep **stops** after a panic (#795) so that
does not repeat.

This RFC chooses **duplicates over loss**. Ownership of a drained batch
stays recoverable across unwind. Only then may the sweep keep running.

It does **not** specify the `PUBLISHED` sidecar, settlement rebuild, or
audit-durability policy. Those are RFC 0055 and RFC 0056.

## 2. Motivation

See current RFC 0053 §2.2. The non-panic paths already requeue. Only
unwind does not. `CLAUDE.md` §3.4 forbids losing acknowledged data and
says nothing against delivering it twice.

## 3. Proposed design

**Invariant.** No acknowledged batch becomes unreachable because a
consumer panicked. After unwind the records (and any *unsettled* audit
events) are back in their buffers. Settled audit events stay out.

**Budget.** At-least-once. Duplicates are tolerated. The bound is
"bounded by snapshot age / in-flight partitions," not "exactly one
extra object per panic." The stronger claim in the 0053 draft fought
the snapshot-rebuild path; this RFC keeps the weaker honest one.

**Mechanism (shape, not types).** Each consuming call takes the batch
in a form whose `Drop` requeues what it has not settled. A caller-side
guard does not cover panic points *inside* the writes. Named types
(`RecoverableBatch`, pool supervisor, publisher respawn) are
implementing-PR details that must satisfy the invariant; they are not
normative API.

**Sweep.** Once the invariant holds, a recovered panic does not
permanently stop the cadence. Cancellation still terminates it.

**Out of scope.** `PUBLISHED` watermarks, tenant settlement, encode-pool
channel policy except as needed to not drop a mined batch on worker
death.

## 4. Alternatives considered

- Keep stopping the sweep (#795). Rejected: the node then requires a
  restart after every publish panic.
- Idempotent object keys. Open in RFC 0055 §7; not required to state
  the invariant.

## 5. Acceptance criteria

> **RFC0054.1 — Recoverable on unwind**
> - **Given** a drained batch and a publish double that panics at the
>   consumer boundary (before audit write, inside it, inside record
>   publish)
> - **When** the panic is recovered
> - **Then** records and unsettled audit events are back in buffers;
>   settled audit events are absent; no WAL mark crosses unrequeued data

> **RFC0054.2 — Sweep survives**
> - **Given** RFC0054.1
> - **When** the next cadence tick runs
> - **Then** the sweep continues; a later healthy barrier publishes

> **RFC0054.3 — Duplicate budget**
> - **Given** a panic after the store accepted at least one partition
> - **When** the batch is published again
> - **Then** every record id is present at least once; extras are
>   allowed and counted; absence is a failure

## 6. Testing strategy

Fault injection at the consumer boundary, parameterised per consumer
(record vs audit). Exact handle types are the implementing PR's.

## 7. Open questions

- Idempotent drain-time object keys (cost: a requeued batch must stay a
  unit). Owned with RFC 0055 if the sidecar needs them.
- Publisher-thread respawn vs disconnected-queue fallback. Implementation.

## 8. References

- RFC 0052, RFC 0053, `#795`, `#796`, `CLAUDE.md` §3.4
- Source quarry: RFC 0053 draft on #802 @ `30a21f80`
