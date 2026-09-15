---
rfc: 0056
title: Audit-sink durability on permanent write failure
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-15
supersedes: —
superseded-by: —
---

# RFC 0056 — Audit-sink durability on permanent write failure

> **Status note.** `drafted`. Split out of RFC 0053's status note so an
> amendment to an *accepted* RFC is not hidden. Source wording: #802 @
> `30a21f80`, status note + the three-way `write_owned` result in §3.2.

## 1. Summary

RFC 0005 §7 says the writer guarantees no audit event is lost across
crashes. The current audit sink reports a *permanent* write failure as
success and `write_ordered` then publishes the dependent records.

This RFC amends that clause: a permanent audit-write failure is a
third outcome. Dependent records for that tenant stay unpublished and
requeued. The tenant becomes server-terminal until repaired.

## 2. Motivation

A record in Parquet without the template event that describes it
breaks `CLAUDE.md` §3.1 (no silent template merges / audit
durability). The 0053 draft changed that behaviour in a status note.
An accepted RFC needs its own amendment and criterion.

## 3. Proposed design

- `write_owned` returns a three-way result: durable / transient /
  permanent.
- `write_ordered` refuses the dependent record publish for that tenant
  on permanent.
- That tenant is marked terminal; other tenants continue.
- Derive failure on the audit side takes the same path.
- Authorization-denial events (`IngestDenied`, RFC 0026) are emitted
  before any frame exists; replay cannot regenerate them. That gap
  stays RFC 0026's, named here so 0056 does not pretend to cover it.

## 4. Alternatives considered

- Keep the silent drop. Rejected against RFC 0005 §7 and §3.1.
- Fold into RFC 0053. Rejected: wrong document.

## 5. Acceptance criteria

> **RFC0056.1**
> - **Given** a permanent audit-write failure for one tenant
> - **When** `write_ordered` returns
> - **Then** that tenant's records are unpublished and requeued, the
>   tenant is terminal, other tenants publish, and the RFC 0005 §7
>   clause is the amended one

## 6. Testing strategy

One fault-injected permanent audit failure beside a healthy second
tenant. Implementing PR.

## 7. Open questions

- Operator repair path for a terminal tenant (restart vs explicit
  clear).
- RFC 0026 `IngestDenied` durability, owned by 0026.

## 8. References

- RFC 0005 §7, RFC 0026, RFC 0053 draft status note on #802

## 9. Extracted wording (RFC 0053 draft, `30a21f80`)

Moved here unedited from the RFC 0053 quarry so the reviewed sentences
survive the split rather than being rewritten from memory. §3 above is
the shape of this RFC and §5 above governs acceptance; the section
numbers inside these paragraphs ("§3.1", "§3.2") and the `RFC0053.n`
ids are the draft's own.

### 9.1 The amendment, as the draft states it (status note)

It **amends accepted RFC 0005's audit-sink durability clause** (§7, "The
writer guarantees no audit event is lost across crashes…"): a permanent
audit write failure stops being a silent drop that reports success and
becomes a third outcome that refuses the dependent record publish and
makes the tenant terminal.

### 9.2 A permanent audit failure is not a settled outcome (draft §3.2)

**"Not drained" has to include a permanent drop, and today it does not.**
`AuditSink::write_owned` reports `fully_durable` as `retained.is_empty()`,
and `route_partition_result` puts a *permanent* write error on neither
path: it counts `permanent_errors`, logs the batch, and retains nothing —
so `write_owned` returns `true`, `write_ordered` proceeds, and the records
whose template events were just dropped are published anyway. That breaks
the ordering the barrier exists to give and, worse, the `CLAUDE.md` §3.1
invariant behind it: a merge with no audit event is exactly the silent
merge the project forbids, and RFC 0005's audit contract says no audit
event is lost. So this RFC **amends the audit sink's permanent-failure
policy** (RFC 0005's audit-sink contract, the flush routing in
`audit_sink.rs`): a permanent audit write failure is *not* a settled
outcome for the dependent records. `write_owned` returns a three-way
result — fully durable, retained-for-retry, or **permanently failed** —
and `write_ordered` refuses the record publish on the third exactly as it
does on the second, requeueing the records rather than publishing them
under a missing event. **The outcome is per tenant, not per batch**: a
drained batch spans tenants, and one tenant's unwritable audit partition
must not stop every other tenant's records from publishing — that would
turn a single bad partition into a node-wide stall. So the result carries
a **failed-tenant set**, the outcome is evaluated per tenant (per audit
partition, which is keyed by tenant and day), and `write_ordered`
publishes the record partitions of every tenant whose audit events are
fully durable while refusing and requeueing only those of the tenants in
that set. Retained-for-retry is already per partition on the existing
path and stays so. **Refusing forever is not a contract, so the third
outcome is terminal for that tenant**, not a retry loop: RFC 0025 §3.3's
quarantine is for permanent `BatchError`s on *data records*, and it works
by writing a `record_quarantined` event — into the very audit sink that is
failing — so it cannot be the escape hatch here. Instead a permanently
failed audit write puts the tenant in the **server-terminal,
client-retryable** class §3.1 already defines: its records stay in the
WAL, unpublished and unacknowledged-past, its appends are refused with
the tenant named, the state is exported and alerted beside
`ourios.wal.tenant_unrecoverable`, and **a restart is the only thing that
clears it**. Not an in-process operator action: the events are gone from
the sink's buffer, so clearing the flag while the process runs would let
the tenant's requeued records publish under events that were dropped —
precisely the ordering the flag exists to protect. A restart is different
in kind, because recovery re-mines the frames from the WAL and RFC 0052's
regeneration-only replay produces those template events again, so the
records publish under events that exist. The rule is therefore stated
flatly: a permanent audit failure is cleared by restart alone, after the
operator has fixed whatever made the store reject writes.

**That argument covers the record-dependent audit stream and nothing
else, which this RFC states rather than over-claims.** The events it
reasons about are the ones the miner regenerates from frames — template
merges and widenings, which accompany published records. RFC 0026's
binding denials do not fit it: `Pipeline::enforce_binding` emits an
`IngestDenied` event to the denial sink and returns the error *before any
frame is appended*, so there is nothing in the WAL for replay to
regenerate and the restart argument simply does not apply. A permanent
drop there is a real gap against RFC 0005 §7's no-loss contract, and it
is **out of this RFC's scope** — its subject is the ordering between
records and the events that describe them, not the denial stream, which
has no records to order against. The gap is named here rather than papered
over, and §7 carries it as a follow-up: denial events need either their
own durable path or an explicit exemption in RFC 0005's contract, and
that is RFC 0026's ground to settle.
The per-partition settlement below reads that third outcome as *not*
settled. RFC0053.2 asserts a permanent audit failure leaving the records
unpublished and requeued and the tenant terminal.

### 9.3 Derive failure, set-aside buffers, and what a restart clears

**The audit sink's derivation failure is not a quarantine, and this RFC
gives it the same terminal treatment as a permanent write.**
`derive_audit_partition` can fail *before* a `PartitionKey` exists — a
timestamp that will not resolve to a partition — so the event never
reaches `route_partition_result` and the sink counts it and drops it.
Ownership-settled, certainly; but treating that as the end of the story
would let the dependent records publish with no template event behind
them, which is the failure §3.2 exists to close and `CLAUDE.md` §3.1
forbids. RFC 0025 §3.3's quarantine cannot serve here either, for the
reason it cannot serve a permanent write failure: it works by writing an
event into the same audit sink. So a derive failure marks that event's
**tenant permanently failed**, exactly as a permanent write does —
`write_owned` reports it in the failed-tenant set, `write_ordered`
refuses that tenant's dependent records and requeues them, and the tenant
enters the server-terminal, client-retryable class.

**A requeue alone would spin, so a terminal tenant's buffers are set
aside.** Requeued records go back into the sink, the next drain picks them
up, the same audit write fails the same way, and the node burns store
calls on a failure nothing in-process can fix. So while a tenant is
terminal its partitions are **excluded from every drain** — the age
sweep's, the barrier's and the publisher's alike — rather than retried:
they stay in the buffers, under the sink's byte accounting, doing nothing
until a restart re-mines the frames behind them. Two consequences follow
and are stated rather than left implicit. Its **WAL frames stay
unsuppressed**: the tenant's publication frontier does not advance, so
recovery replays them, which is what makes the set-aside lossless rather
than a quiet drop. And **the barrier does not advance past them**: the
tenant's own frontier holds where it is, so no checkpoint or snapshot
mark claims its records are durable — the node-wide checkpoint still
advances for every healthy tenant, per §3.2's per-tenant horizon.
RFC0053.2 asserts it: a terminal tenant's partitions are not re-drained,
its records are still in the WAL after a restart, and its frontier has
not moved.

**What a restart clears, stated precisely, because "a restart clears it"
is too strong on its own.** A restart is the *mechanism* — recovery
re-mines the frames and regeneration produces the events again — but it
cures nothing by itself: it clears the state only once the **underlying
condition is repaired**, and the two conditions differ. A permanent write
failure needs the audit store to accept writes again. A derive failure
needs the clock, because the event's timestamp is **not carried in the
frame**: `MinerCluster` stamps every audit event from its own wall-clock
source (`self.clock.now()`), so the failing timestamp is a property of
when the event was emitted, not of the record being mined. A restart
therefore re-mines the same frame and stamps a *fresh* timestamp, which
derives normally on a node whose clock has recovered — and fails again,
identically, on one whose clock is still wrong, which is the same shape as
a store that is still rejecting. So the rule is one sentence: the terminal
state clears on the first restart *after* the condition behind it is
repaired, and a restart into an unrepaired condition simply re-enters it,
visibly, on the same alert. Nothing is lost either way, because the
records stay in the WAL unpublished for as long as the state holds.
RFC0053.2 covers the derive failure beside the write failure.

### 9.4 Draft open question (source for §7)

- [ ] Where RFC 0026's binding-denial events go when the audit store
      rejects writes permanently. §3.2's three-way outcome and its terminal
      rule cover the record-dependent stream only, because a denial is
      emitted before any frame exists and replay cannot regenerate it — so
      a permanent drop there is an open gap against RFC 0005 §7. It needs
      either a durable path of its own or an explicit exemption in that
      contract; either is RFC 0026's to settle, not this RFC's.

### 9.5 Draft references (§8)

- RFC 0005 §7 (audit files and their durability clause) — **amended by §3.2
  of this RFC**: the audit sink's permanent-failure path reports a third
  outcome rather than success, `write_ordered` refuses the dependent record
  publish on it, and the tenant becomes terminal until a **restart** clears
  it — restart alone, per §3.2, because the dropped events are out of the
  sink's buffer and only recovery's re-mining regenerates them. Without the amendment a dropped event is reported as
  fully durable and its records publish anyway, which breaks both that
  clause and `CLAUDE.md` §3.1.
- RFC 0025 §3.3 (permanent-encode quarantine) — the disposition for a
  poisoned *data record*; §3.2 states why it cannot serve an audit-write
  failure, and §3.2's watermark rule states how a quarantined record is
  covered.
