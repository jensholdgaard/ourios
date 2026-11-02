---
rfc: 0067
title: "meta: broker-replicated acks under WAL-before-ack"
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-11-02
supersedes: —
superseded-by: —
---

# RFC 0067 — meta: broker-replicated acks under WAL-before-ack

> **Status: `drafted`.** This is a `meta:` RFC. It proposes a change to
> `AGENTS.md` §3.4, written against the text **RFC 0064** (`accepted`)
> put there, and to `docs/hazards.md` H3 and H8. `AGENTS.md`'s footer
> requires "a `meta:` RFC and majority maintainer approval" for any
> change to it, and `docs/hazards.md` §Adding a new hazard requires one
> for a changed hazard contract. Per `AGENTS.md` §8.5 the edits are
> **not** made in the drafting session: §3 gives the exact text, and it
> lands in a separate PR once the maintainer approves this RFC.
> Precedent: RFC 0012, RFC 0057, RFC 0064. It is the amendment **RFC
> 0066** (`drafted`, Kafka WAL backend) §3.12 asks for; RFC 0066 cannot
> reach `red` before this RFC is accepted, and carries the runtime
> criteria this rule requires (§5).

## 1. Summary

`AGENTS.md` §3.4 says "durably" means fsync or equivalent and allows
"no in-memory-only acks, ever"; RFC 0064's replication bullet requires
at least two replicas to have **fsynced** a batch before the ack. A
Kafka-protocol broker acknowledges once its write quorum holds the
batch, and Apache Kafka by default holds it in page cache, not on disk.
This RFC lets a broker-backed WAL meet RFC 0064's bullet through the
broker's own quorum and leader election, checked by Ourios from the
broker's configuration at startup and periodically while running. A
broker that verifiably persists before it acknowledges meets the bullet
unchanged. For one that does not, only the fsync is waived, and only
under an explicit operator opt-in, with a startup warning and replicas
in separate failure domains; the two-replica floor, leader safety and
fencing are never waived.

## 2. Motivation

RFC 0066 makes a Kafka-protocol topic the receiver's WAL, so receivers
keep no disk (RFC 0066 §2.2). Its ack waits for the broker, not for an
fsync Ourios can see. RFC 0064's bullet applies to any replicated WAL,
so RFC 0066 as drafted breaks §3.4 on the default Apache Kafka
configuration. Either the rule says what a broker must guarantee, or
the backend cannot ship. The rule belongs next to the invariant, not
inside RFC 0066, so a later broker backend is held to the same bar.

## 3. Proposed design

### 3.1 What a broker acknowledgement guarantees

| Failure | Broker that does not fsync first (Kafka default) | Broker that persists before ack |
|---|---|---|
| One broker process crash | survives: the rest of the write quorum holds the batch | survives |
| One disk lost | survives with a quorum of ≥ 2 | survives |
| One machine loses power | Kafka: survives only if the replica that lost its page cache cannot lead; with Eligible Leader Replicas (KIP-966) it tolerates `min.insync.replicas − 1` such shutdowns, without it a sole remaining in-sync leader that restarts empty is re-elected and the others truncate | survives |
| Every replica in the write quorum loses power before writeback | **acknowledged batches lost** | survives |
| Unclean leader election | **acknowledged batches lost** | **acknowledged batches lost** |

Sources are in §8. Kafka's design states it does "not require that
crashed nodes recover with all their data intact" and does "not want to
require the use of fsync on every write"; its topic docs recommend
"use replication for durability" over `flush.messages`. Redpanda, a
Raft log, "fsyncs each acks=all batch to disk on a majority of replicas
before it acknowledges" unless write caching is on, which is off by
default in production mode. WarpStream "never acknowledges writes until
data is durably persisted in object storage and committed to the
metadata store".

The gap is the fourth row: a power loss that reaches the whole write
quorum before the kernel writes its pages out. Separate failure domains
(Kafka `broker.rack`, Redpanda `rack`) turn one rack or zone event into
several independent ones.

### 3.2 `AGENTS.md` §3.4

The opening paragraph,

```markdown
Ingester acknowledges an OTLP batch only after it has been durably
written to the WAL. "Durably" means fsync or equivalent. No in-memory-only
acks, ever.
```

becomes:

```markdown
Ingester acknowledges an OTLP batch only after it has been durably
written to the WAL. "Durably" means fsync or equivalent. No in-memory-only
acks, except the opt-in broker case in the rules below.
```

RFC 0064's replication bullet, as enacted,

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

becomes (changes: the broker's write quorum named as its floor, and
the exception named at the end):

```markdown
- Replication is in addition to the WAL, not instead of it. A
  replicated WAL acknowledges a batch only after at least two
  replicas, and at least its configured floor (on a broker, its write
  quorum), have fsynced it, so losing one disk never loses an
  acknowledged batch; when that cannot be met, the batch is rejected,
  not acknowledged on fewer. A replica becomes leader only if it
  already holds every acknowledged batch. A replica that has lost
  leadership can never commit another batch, whatever its clock says,
  so a success it sends late is only ever for a batch the new leader
  holds. The only exception is the fsync, and
  only for an opted-in broker-backed WAL, as the next rule says.
```

and a bullet follows it:

```markdown
- A broker-backed WAL (a Kafka-protocol log) meets the rule above
  through the broker's write quorum and leader election, which Ourios
  checks from the broker's configuration at startup and periodically
  while running; a broker whose configuration cannot be read does not
  start. If Ourios cannot verify that the broker fsyncs, or commits to
  object storage, before it acknowledges, the replicas hold the batch
  only in memory: the backend then acknowledges only when the operator
  has opted in by configuration, warns at startup, and has every
  partition's replicas in distinct failure domains. The opt-in waives
  the fsync only, never the two-replica floor, leader safety or
  fencing. A check that fails while running stops acknowledgements on
  that topic with a retryable rejection until it passes again, or, for
  a lost fsync guarantee, until the opt-in covers it.
```

The rest of §3.4 and §4 item 3 (crash test for every backend) are
unchanged.

### 3.3 `AGENTS.md` footer

```markdown
2026-MM-DD revision adds the broker case to §3.4: a Kafka-protocol WAL
meets the replication rule through the broker's write quorum and leader
election, checked at startup and while running, and an acknowledgement
the broker does not verifiably persist first needs an operator opt-in,
a startup warning and replicas in separate failure domains, per **RFC
0067** (maintainer-approved `meta:` RFC).
```

and `*Last updated: …*` moves to the enacting date.

### 3.4 `docs/hazards.md`

- **H3, first mitigation.** "An ack is emitted **only after** fsync (or
  equivalent durability primitive) on the WAL." becomes: "An ack is
  emitted **only after** fsync (or equivalent durability primitive) on
  the WAL. The one exception is an opted-in broker backend (RFC 0066,
  RFC 0067) whose broker Ourios cannot verify persists before
  acknowledging: there the ack waits for the broker's write quorum of
  at least two replicas, in distinct failure domains, holding the batch
  in memory, and a power loss across that whole quorum before writeback
  loses acknowledged batches."
- **H3, replication bullet.** RFC 0064's bullet gains a final sentence:
  "On the broker backend (RFC 0066) the same floor and leader rules
  hold through the broker's quorum and election, re-checked while
  running; a failed check stops acknowledgements on that topic."
- **H3, escalation.** "Ack-without-fsync ever observed in code review →
  P0 bug, hotfix path." becomes: "Ack-without-fsync ever observed in
  code review → P0 bug, hotfix path, except on a broker backend whose
  operator has opted in (RFC 0067); a broker backend acknowledging
  without verified persistence and without the opt-in, or after a
  failed runtime check, is the same P0."
- **H8, banner.** "This hazard is dormant on the local WAL backend and
  live on the replicated one (RFC 0065)." becomes "This hazard is
  dormant on the local WAL backend and live on the replicated (RFC
  0065) and broker (RFC 0066) backends." No mitigation changes; RFC
  0066 §3.6 dedups by log position, which H8 allows.

### 3.5 How the rule maps to brokers

RFC 0066 owns the checks; this table is what they must establish.

| Rule | Apache Kafka | Redpanda (Raft) | WarpStream |
|---|---|---|---|
| ≥ 2 replicas and the write quorum hold the batch | `acks=all`, `min.insync.replicas ≥ 2`, every partition with ≥ `max(2, min.insync.replicas)` replicas | `acks=all` (majority), every partition with ≥ 2 replicas | not verifiable (object storage replicates internally) |
| Leader holds every acked batch | `unclean.leader.election.enable=false`; with the opt-in, also ELR (§7 Q5) | Raft election | single metadata store |
| fsync before ack (verified) | `flush.messages=1` (§7 Q4) | `write.caching=false` | not exposed: opt-in |

A broker that cannot show a row's setting is classified conservatively:
a broker that does persist but cannot show it needs the opt-in, never
the reverse.

**Lifetime.** The checks run at startup and every **60 s** while
running; 60 s is a starting value, tuned by the re-check outcome metric
RFC 0066 §3.9 names, against the rate of config changes it observes.
Each check fails closed:

- the configuration cannot be read at startup → the receiver does not
  start;
- the floor or leader check fails, or the config cannot be read while
  running → the topic stops acknowledging (retryable rejection, RFC
  0053 §3.1) until a check passes;
- the fsync check moves from verified to unverified and the opt-in is
  not set → the same; with the opt-in set → acknowledgements continue
  and a WARN event is emitted.

Between two checks an admin change can go unseen for up to one
interval; the opt-in's warning and H3's escalation cover that window,
and the interval is the knob that shrinks it.

### 3.6 What does not change

- RFC 0064's floor of two, leader rule and fencing, for every backend.
- `AGENTS.md` §3.6: the broker holds records only up to the WAL
  horizon; object storage stays the source of truth.
- The local and replicated (RFC 0065) backends: fsync, as today.

## 4. Alternatives considered

**Require broker persistence, no opt-in.** Accept only brokers §3.5
classifies as verified. It keeps "no in-memory-only acks, ever" word for
word. Kafka's design puts fsync on every write at "two to three orders
of magnitude" slower; RFC 0066's batch window produces one record batch
per partition per window, so `flush.messages=1` costs one fsync per
broker per partition per window, close to the local WAL's group commit,
but it applies to the whole topic and Kafka operators rarely accept it.
WarpStream would be excluded. Close second; §7 Q1 is this choice.

**Forbid the broker backend.** RFC 0065 already gives replication with
fsync and no external system. It throws away RFC 0066's reason to
exist: teams that already run Kafka and want stateless receivers.

**Allow replication-only acks with no opt-in.** The earlier draft of RFC
0064. It matches Kafka's own bar, but changes what §3.4 promises on
every deployment that picks the backend, silently.

**Check at startup only.** Simpler, but an admin can flip
`flush.messages` or `write.caching` after start and the receiver keeps
acknowledging under a guarantee it no longer has.

**An all-in-sync-replicas rule.** The first draft of this RFC. It fits
Kafka's ISR but not a Raft broker, which acknowledges on a majority;
the write-quorum wording fits both.

## 5. Acceptance criteria

These check the enactment only, as RFC 0064 §5 does. The runtime
behaviour the rule requires (startup classification, the opt-in and its
warning, the failure-domain check, the periodic re-check and its
verified-to-unverified transition) is specified as criteria
RFC0066.16–.20 in RFC 0066 §5 and tested there.

> **Scenario RFC0067.1 — `AGENTS.md` §3.4 matches §3.2–§3.3**
> - **Given** the enacting PR
> - **When** `AGENTS.md` is diffed against the version before it
> - **Then** §3.4's opening paragraph, the replication bullet and the
>   new broker bullet are exactly §3.2's text, the footer carries
>   §3.3's sentence and a new `Last updated` date, and nothing else in
>   the file changes

> **Scenario RFC0067.2 — `docs/hazards.md` matches §3.4**
> - **Given** the enacting PR
> - **When** `docs/hazards.md` is diffed against the version before it
> - **Then** H3's first mitigation, replication bullet and escalation
>   and H8's banner read as §3.4 gives them, and no other hazard changes

> **Scenario RFC0067.3 — citations still resolve**
> - **Given** the enacted `AGENTS.md`
> - **When** `just agents-citations` and `mdbook build` run
> - **Then** both pass

## 6. Testing strategy

- RFC0067.1, .2: maintainer review of the enacting PR's diff against
  §3.2–§3.4; the text is fixed here, so the review is a comparison.
- RFC0067.3: the existing `book` CI job.
- The behaviour is tested by RFC0066.16–.20 (RFC 0066 §6).
- The fourth row of §3.1 (correlated power loss) is not testable in CI;
  it is the risk the opt-in accepts, not a property Ourios claims.

## 7. Open questions

Behaviour decisions for the maintainer. Each has a recommendation.

- [ ] **Q1. May Ourios confirm data that the brokers hold only in
  memory?** On default Apache Kafka, an acknowledged batch sits in RAM
  on two or three machines until the kernel writes it out; if all of
  them lose power together, it is gone. Options: (a) allow only with an
  explicit opt-in and a startup warning (this draft); (b) never: accept
  only brokers that show they write to disk first; (c) allow without an
  opt-in. *Recommendation: (a).* It keeps "no in-memory-only acks" as
  the default, makes the operator choose the trade knowingly, and still
  lets Kafka shops use the backend. (b) is the safe fallback if you
  want the invariant unchanged in substance.
- [ ] **Q2. With the opt-in, must each copy be in a different rack or
  zone?** *Recommendation: yes, checked from the brokers' rack labels,
  refusing a missing label.* Without it, one power event in one rack
  loses data, which is the exact failure the opt-in asks the operator
  to accept as unlikely.
- [ ] **Q3. Two copies in the write quorum, or three?** RFC 0064 fixed
  two as the minimum. *Recommendation: keep two in the rule and
  recommend three replicas (Kafka `min.insync.replicas=2`) in the
  operator docs and Helm defaults.* Three keeps accepting writes while
  one broker is down for maintenance, an availability choice, not a
  durability floor.
- [ ] **Q4. On Apache Kafka, does `flush.messages=1` count as "writes to
  disk first"?** The code path fsyncs each replica before the ack, but
  the documentation does not promise it. *Recommendation: yes,* with a
  CI container test against Kafka that would catch an upstream change.
- [ ] **Q5. With the opt-in on Apache Kafka, require Eligible Leader
  Replicas?** Without ELR, one machine's power loss can lose
  acknowledged data when it was the last in-sync copy (§3.1, row 3),
  which RFC 0064's leader rule forbids. ELR is on by default only on new
  clusters from Kafka 4.1. *Recommendation: yes,* checked from the
  broker's finalized features; Redpanda and WarpStream do not use
  Kafka's ISR election.
- [ ] **Q6. How quickly must Ourios notice that the broker's settings
  changed?** *Recommendation: re-check every 60 s to start, and tune it
  from the re-check metric.* Shorter costs one metadata and config
  request per topic per interval; longer leaves a wider window in which
  a changed broker keeps getting acknowledged writes.

## 8. References

- `AGENTS.md` §3.4, §3.6, §4, §8.5, footer
- `docs/hazards.md` H3, H8, §Adding a new hazard
- RFC 0012, RFC 0057, RFC 0064 (`meta:` precedent; RFC 0064 §3.1 is the
  base text), RFC 0053 §3.1 (retryable rejection), RFC 0065 (replicated
  WAL), RFC 0066 §3.1, §3.2, §3.4, §3.9, §3.12, §5 (Kafka backend)
- Apache Kafka 4.1 documentation, *Topic configs*
  (`flush.messages`, `flush.ms`, `min.insync.replicas`,
  `unclean.leader.election.enable`):
  <https://kafka.apache.org/41/configuration/topic-configs/>
- Apache Kafka 4.1 documentation, *Design*, §Replication ("a committed
  message will not be lost, as long as there is at least one in sync
  replica alive"; no fsync requirement):
  <https://kafka.apache.org/41/design/design/>
- Apache Kafka 4.1 documentation, *Eligible Leader Replicas* (on by
  default for new clusters from 4.1, not in 4.0):
  <https://kafka.apache.org/41/operations/eligible-leader-replicas/>
- KIP-966, *Eligible Leader Replicas* (unflushed page-cache loss on
  unclean shutdown; "last replica standing"; `MinISR−1` tolerance):
  <https://cwiki.apache.org/confluence/display/KAFKA/KIP-966%3A+Eligible+Leader+Replicas>
- Redpanda documentation, *Configure topics*, §Write caching (fsync on a
  majority before an `acks=all` ack; write caching off in production
  mode): <https://docs.redpanda.com/current/develop/manage-topics/config-topics/>
- WarpStream documentation, *Write path* (ack after object-store write
  and metadata commit):
  <https://docs.warpstream.com/warpstream/overview/architecture/write-path>
- Alagappan et al., *Correlated Crash Vulnerabilities*, OSDI 2016
  (correlated crashes found to lose data in widely used distributed
  stores, Kafka among those studied):
  <https://www.usenix.org/conference/osdi16/technical-sessions/presentation/alagappan>
- Linux kernel documentation, `vm.dirty_expire_centisecs` and
  `vm.dirty_writeback_centisecs` (background writeback of page cache):
  <https://docs.kernel.org/admin-guide/sysctl/vm.html>
- Issue #1141
