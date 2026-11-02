---
rfc: 0064
title: "meta: replication behind WAL-before-ack"
status: accepted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-11-01
supersedes: —
superseded-by: —
---

# RFC 0064 — meta: replication behind WAL-before-ack

> **Status note. Accepted (2026-11-02).** Maintainer-approved on
> 2026-11-02 and enacted the same day, per §7 Q1: `AGENTS.md` §3.4's
> replication bullet, §4 item 3 and the footer took §3.1–§3.3's text,
> and `docs/hazards.md` H3 and H8 took §3.4's. The note below is the
> drafting-time status.
>
> **Status: `specified`.** Tracks #1141 (option 2). This is a `meta:` RFC.
> It proposes changes to `AGENTS.md` §3.4 and §4 and to
> `docs/hazards.md` H3 and H8. `AGENTS.md`'s footer requires "a `meta:`
> RFC and majority maintainer approval" for any change to it, and
> `docs/hazards.md` §Adding a new hazard requires one for a changed
> hazard contract. Per `AGENTS.md` §8.5 the edits are **not** made in
> the drafting session: §3 gives the exact text, and it lands in a
> separate PR as soon as the maintainer approves this RFC, not when RFC
> 0065 ships (§7, Q1). Precedent: RFC 0012 and RFC 0057. The
> implementation is RFC 0065, which cannot reach `red` before this RFC
> is accepted.

## 1. Summary

`AGENTS.md` §3.4 already allows replication "in addition to the WAL,
not instead of it". RFC 0065 proposes exactly that: each replica keeps
a local, fsynced WAL, and a batch is acknowledged only once enough
replicas have fsynced it. This RFC does not change what "durably"
means. It writes down what the existing sentence requires of a
replicated WAL, so the rule is checkable rather than implied, and
states it without naming a protocol, so it binds an in-sync-set design
and a Raft quorum alike, as three separate guarantees: an ack waits
until at least two replicas, and at least the backend's configured
floor, have fsynced the batch; a
replica becomes leader only if it already holds every acknowledged
batch; and a replica that has lost leadership can never commit
another batch, so a success it sends late is only ever for a batch the
new leader holds.
The crash test covers the replicated backend too. H8 goes from dormant
to live for the replicated backend, with the dedup key RFC 0065
defines.

An earlier draft of this RFC widened "durably" to a Kafka broker's
acknowledgement, which is replication without fsync. That widening is
no longer proposed here; RFC 0066 (the Kafka backend) would need its
own `meta:` amendment.

## 2. Motivation

### 2.1 The sentence exists, the rules do not

§3.4's last rule says only "If you add replication later, it is in
addition to the WAL, not instead of it." It says nothing about when a
replicated write may be acknowledged, how many replicas are enough, or
what stops an old leader from acknowledging after it has been replaced.
Each of those is a way to lose acked data while keeping to the letter
of the rule. A reviewer of RFC 0065's code needs them stated here, not
inferred.

### 2.2 Why replication at all

A local fsync survives a process crash and a power loss on that node.
It does not survive losing the node's disk, which takes every acked
record not yet in Parquet with it (#1141). Replicating the WAL to other
nodes, each fsyncing its own copy, survives losing any one disk
without weakening the fsync rule.

## 3. Proposed design

### 3.1 `AGENTS.md` §3.4, the replication bullet

The last bullet of §3.4's rules,

```markdown
- If you add replication later, it is in addition to the WAL, not
  instead of it.
```

becomes:

```markdown
- Replication is in addition to the WAL, not instead of it. A
  replicated WAL acknowledges a batch only after at least two
  replicas, and at least its configured floor, have fsynced it, so
  losing one disk never loses an acknowledged batch; when that cannot
  be met, the batch is rejected, not acknowledged on fewer. A replica
  becomes leader only if it already holds every acknowledged batch. A
  replica that has lost leadership can never commit another batch,
  whatever its clock says, so a success it sends late is only ever for
  a batch the new leader holds.
```

The rest of §3.4 is unchanged. The crash-recovery bullet already says
"an ingester killed mid-batch must recover without losing acknowledged
data"; §3.2 adds that this covers every backend.

### 3.2 `AGENTS.md` §4, item 3

```markdown
3. **WAL durability vs. latency** — batched fsync with explicit knob,
   crash-recovery test in CI for every WAL backend, including a leader
   killed mid-batch on the replicated one.
```

### 3.3 `AGENTS.md` footer

A dated sentence in the footer's changelog, in the form RFC 0012 and
RFC 0057 used:

```markdown
2026-MM-DD revision states what §3.4's "replication is in addition to
the WAL" requires of a replicated WAL (at least two fsynced replicas
before the ack, a new leader holding every acknowledged batch, fencing
of a replaced leader) and extends §4
item 3's crash test to every backend, per **RFC 0064**
(maintainer-approved `meta:` RFC).
```

and `*Last updated: …*` moves to the enacting date.

### 3.4 `docs/hazards.md` H3 and H8

- **H3.** The mitigation bullet "Replication, when added, is **in
  addition to** the WAL, not a replacement." becomes: "Replication is
  **in addition to** the WAL, not a replacement. On the replicated
  backend (RFC 0065) an ack waits for at least two fsynced replicas
  (its write quorum), a replica becomes leader only if it holds every
  acknowledged batch, a replaced leader cannot commit another batch
  (a success it sends late is only for a batch the new leader holds),
  and the crash-recovery test kills the leader mid-batch."
- **H8.** The banner's "Ourios does not currently replicate at ingest,
  so this hazard is dormant" becomes "This hazard is dormant on the
  local WAL backend and live on the replicated one (RFC 0065)." The
  *Currently* bullet becomes: "Dormant on the local backend, which does
  not replicate. **Live on the replicated backend**: a new leader or
  flusher replaying from the last checkpoint can write a record twice,
  and RFC 0065 removes those copies with a key the log assigns once
  (each record's group, sequence number and index in its frame), never
  a time window." The *When replication is proposed* bullet, which today
  requires a producer-supplied idempotency key, is split in two:
  - "*Replication duplicates* — copies the replication layer itself
    makes by replay, redelivery or handoff — MUST be deduplicated by a
    key assigned once per acknowledged record, by the producer or by the
    log, never by a time window."
  - "*Client-retry duplicates* — a client resending after a lost ack —
    are not caused by replication and exist on the local backend too.
    Removing them needs a producer-supplied idempotency key; until an
    RFC adds one, they are the same as on the local backend, and a
    replicated backend MUST NOT make them more frequent."

  The *Detection* signals become required for the replicated backend
  (RFC 0065 §6.9).

### 3.5 What does not change

- What "durably" means: fsync or equivalent, on every replica that
  counts toward the ack.
- `AGENTS.md` §3.6: object storage stays the source of truth, and local
  disk stays cache and WAL. RFC 0065 keeps its leadership records and
  checkpoints in the store.
- `AGENTS.md` §1: Ourios still ships one binary and a Helm chart. The
  replicated WAL needs no external system beyond the object store every
  deployment already has.
- RFC 0008 (`accepted`) stays the local WAL's specification.

## 4. Alternatives considered

**Change nothing and let RFC 0065 carry the rules.** The rules would
bind only RFC 0065's implementation, and the next replication proposal
could ignore them. They belong next to the invariant.

**Widen "durably" to a broker's acknowledgement.** The earlier draft of
this RFC. A Kafka broker acknowledges on replication, not on fsync, so
a correlated power loss can lose acked records. RFC 0065 needs no
widening, so this RFC does not make one; RFC 0066 must argue for it on
its own.

**Allow a floor of 1, or leave the floor to configuration.** The leader
alone would then acknowledge, which is today's local guarantee and is
what the local backend is for; a replicated backend configured that way
loses acknowledged data with one disk and stops meaning safe. Rejected
(maintainer, 2026-11-02, §7 Q2): the rule fixes a minimum of two. RFC
0065 never goes below it (a majority of 3 or 5, RFC 0065 §3.8), so the
minimum binds future backends such as RFC 0066.

## 5. Acceptance criteria

> **Scenario RFC0064.1 — `AGENTS.md` §3.4 and §4 match §3.1–§3.3**
> - **Given** the enacting PR
> - **When** `AGENTS.md` is diffed against the version before it
> - **Then** §3.4's last rule is exactly §3.1's text, §4 item 3 is
>   exactly §3.2's text, the footer carries §3.3's sentence and a new
>   `Last updated` date, and nothing else in the file changes

> **Scenario RFC0064.2 — `docs/hazards.md` matches §3.4**
> - **Given** the enacting PR
> - **When** `docs/hazards.md` is diffed against the version before it
> - **Then** H3's replication bullet, H8's banner and H8's *Currently*
>   bullet read as §3.4 gives them, and no other hazard changes

> **Scenario RFC0064.3 — citations still resolve**
> - **Given** the enacted `AGENTS.md`
> - **When** `just agents-citations` and `mdbook build` run
> - **Then** both pass; §3.4 keeps its number, so no citation moves

## 6. Testing strategy

- RFC0064.1 and .2: maintainer review of the enacting PR's diff against
  §3.1–§3.4; the text is fixed here so the review is a comparison, not a
  judgement.
- RFC0064.3: the existing `book` CI job runs both checks.
- The behaviour the new rule requires is tested by RFC 0065's criteria
  (RFC0065.2–.6), not by this RFC.

## 7. Open questions

Both resolved by the maintainer on 2026-11-02.

- [x] **Q1. When should the project rules change?** **On approval.**
  The enacting PR lands as soon as this RFC is approved, so the new
  rule is the bar the RFC 0065 implementation PRs are reviewed against;
  for the local backend it says what the old rule said.
- [x] **Q2. With replication on, may Ourios confirm data that only one
  disk holds?** **No: a minimum of two, fixed in `AGENTS.md`** (§3.1).
  Without replication one disk is all there is, and that stays
  allowed. RFC 0065 already cannot go lower, so the minimum binds only
  future backends, Kafka included (RFC 0066).

## 8. References

- `AGENTS.md` §1, §3.4, §3.6, §4, §8.5, footer
- `docs/hazards.md` H3, H8, §Adding a new hazard
- RFC 0008 (local WAL), RFC 0012 and RFC 0057 (`meta:` precedent),
  RFC 0065 (replicated WAL), RFC 0066 (Kafka backend)
- Issue #1141
