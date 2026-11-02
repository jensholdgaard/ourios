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
> `AGENTS.md` §3.4, on top of the text **RFC 0064** fixes, and to
> `docs/hazards.md` H3 and H8. `AGENTS.md`'s footer requires "a `meta:`
> RFC and majority maintainer approval" for any change to it, and
> `docs/hazards.md` §Adding a new hazard requires one for a changed
> hazard contract. Per `AGENTS.md` §8.5 the edits are **not** made in
> the drafting session: §3 gives the exact text, and it lands in a
> separate PR once the maintainer approves this RFC. Precedent: RFC
> 0012, RFC 0057, RFC 0064. It is the amendment **RFC 0066** (`drafted`,
> Kafka WAL backend) §3.12 asks for; RFC 0066 cannot reach `red` before
> this RFC is accepted.

## 1. Summary

`AGENTS.md` §3.4 says "durably" means fsync or equivalent, and "no
in-memory-only acks, ever". A Kafka-protocol broker acknowledges a
produce with `acks=all` once every in-sync replica has the record, and
Apache Kafka by default holds it in page cache, not on disk, on every
one of them. This RFC adds a broker case to §3.4: a broker
acknowledgement counts as durable when it comes from every in-sync
replica, at least two are in sync, and a replica that may lack an
acknowledged batch cannot become leader. A broker that persists before
it acknowledges (Redpanda's default, Kafka with `flush.messages=1`,
WarpStream's object-store commit) meets the rule as it stands. A broker
that does not is allowed only by an explicit operator opt-in, with a
startup warning and replicas in separate failure domains, because then
a power loss across every in-sync replica at once loses acknowledged
batches.

## 2. Motivation

RFC 0066 makes a Kafka-protocol topic the receiver's WAL, so receivers
keep no disk (RFC 0066 §2.2). Its ack path (§3.4) waits for `acks=all`,
not for an fsync Ourios can see. After RFC 0064, §3.4 allows replication
only in addition to an fsynced WAL, so RFC 0066 as drafted breaks the
invariant on the default Apache Kafka configuration. Either the rule
says what a broker must guarantee, or the backend cannot ship. The rule
belongs next to the invariant, not inside RFC 0066, so a later broker
backend is held to the same bar.

## 3. Proposed design

### 3.1 What a broker acknowledgement guarantees

| Failure | Kafka, default (no broker fsync) | Broker that persists before ack |
|---|---|---|
| One broker process crash | survives: the other in-sync replicas hold the batch | survives |
| One disk lost | survives with ≥ 2 in sync | survives |
| One machine loses power | survives only if the replica that lost its page cache cannot lead: with Eligible Leader Replicas (KIP-966), Kafka tolerates `min.insync.replicas − 1` such shutdowns; without it, a sole remaining in-sync leader that restarts empty is re-elected and the others truncate | survives |
| Every in-sync replica loses power before writeback | **acknowledged batches lost** | survives |
| Unclean leader election | **acknowledged batches lost** | **acknowledged batches lost** |

Sources for each row are in §8. Kafka's design states it does "not
require that crashed nodes recover with all their data intact" and
does "not want to require the use of fsync on every write"; its topic
docs recommend "use replication for durability" over `flush.messages`.
Redpanda "fsyncs each acks=all batch to disk on a majority of replicas
before it acknowledges" unless write caching is on, which is off by
default in production mode. WarpStream "never acknowledges writes until
data is durably persisted in object storage and committed to the
metadata store".

The gap is the fourth row: a power loss that reaches every in-sync
replica before the kernel writes their pages out. Placing replicas in
separate failure domains (Kafka `broker.rack`, Redpanda `rack`) turns
it from one rack or zone event into several independent ones.

### 3.2 `AGENTS.md` §3.4

The opening paragraph, as it stands after RFC 0064,

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

and a bullet is added after RFC 0064's replication bullet:

```markdown
- A broker-backed WAL (a Kafka-protocol log) acknowledges a batch only
  after every in-sync replica holds it, with at least two in sync, and
  only on a topic where a replica that may lack an acknowledged batch
  cannot become leader (no unclean leader election); when that cannot
  be met, the batch is rejected, not acknowledged on fewer. If Ourios
  cannot verify that the broker writes the batch to disk or object
  storage before acknowledging it, the acknowledgement is in memory on
  every replica, so the backend starts only when the operator has
  opted in by configuration, logs a warning at startup, and refuses a
  topic with two replicas of one partition in the same failure domain.
```

The rest of §3.4, including RFC 0064's text and §4 item 3's
every-backend crash test, is unchanged.

### 3.3 `AGENTS.md` footer

```markdown
2026-MM-DD revision adds a broker case to §3.4: a Kafka-protocol WAL
acknowledges from every in-sync replica with at least two in sync and
no unclean election, and an acknowledgement the broker does not persist
first needs an operator opt-in, a startup warning and replicas in
separate failure domains, per **RFC 0067** (maintainer-approved `meta:`
RFC).
```

and `*Last updated: …*` moves to the enacting date.

### 3.4 `docs/hazards.md`

- **H3.** A mitigation bullet is added after the replication bullet:
  "On the broker backend (RFC 0066) an ack waits for `acks=all` with
  `min.insync.replicas ≥ 2` and no unclean leader election. When Ourios
  cannot verify that the broker persists before acknowledging, the ack
  is in page cache on every replica: the backend then needs an operator
  opt-in, warns at startup, and requires each partition's replicas in
  distinct failure domains; a power loss across all in-sync replicas
  before writeback still loses acknowledged batches." The *Escalation*
  line gains: "Broker backend running in opt-in mode without its
  startup warning, or on a topic that fails the checks → P0 bug."
- **H8.** The banner, after RFC 0064, gains the broker backend: "This
  hazard is dormant on the local WAL backend and live on the replicated
  (RFC 0065) and broker (RFC 0066) backends." No mitigation changes;
  RFC 0066 §3.6 dedups by log position, which H8 as RFC 0064 words it
  allows.

### 3.5 How Ourios decides "persists before acknowledging"

At startup the broker backend reads the topic's effective configuration
(DescribeConfigs, RFC 0066 §3.2):

- `write.caching` present and `false` (Redpanda) → persisted;
- otherwise `flush.messages` equal to `1` (Apache Kafka) → persisted;
- anything else, including a broker that exposes neither key
  (WarpStream, Redpanda before 24.1) → not verified, opt-in required.

The classification is conservative: a broker that does persist but
cannot show it needs the opt-in, never the reverse. On Apache Kafka,
`flush.messages=1` makes a follower fsync inside its append, before
the fetch that lets the leader's high watermark pass the batch; the
documentation does not state this ordering, so §7 Q4 asks whether to
rely on it.

### 3.6 What does not change

- RFC 0064's text, including its minimum of two replicas and its leader
  and fencing rules, which the broker case restates in broker terms.
- `AGENTS.md` §3.6: the broker holds records only up to the WAL
  horizon; object storage stays the source of truth.
- The local and replicated (RFC 0065) backends: fsync, as today.

## 4. Alternatives considered

**Require broker persistence, no opt-in.** Accept only topics §3.5
classifies as persisted. It keeps "no in-memory-only acks, ever" word
for word. Kafka's design puts fsync on every write at "two to three
orders of magnitude" slower; RFC 0066's batch window produces one
record batch per partition per window, so `flush.messages=1` costs one
fsync per broker per partition per window, close to the local WAL's
group commit, but it applies to the whole topic and teams running Kafka
rarely accept it. WarpStream would be excluded unless it exposes a
checkable key. Close second to this RFC's design; §7 Q1 is exactly
this choice.

**Forbid the broker backend.** RFC 0065 already gives replication with
fsync and no external system. It throws away RFC 0066's reason to exist:
teams that already run Kafka and want stateless receivers.

**Allow replication-only acks with no opt-in.** The earlier draft of RFC
0064. It matches Kafka's own bar, but changes what §3.4 promises on
every deployment that picks the backend, silently. The opt-in and the
warning make the operator the one who accepts the fourth row of §3.1.

**Treat Ourios's own retry as a fix.** Ourios cannot re-send what it
has already acknowledged; the client has dropped it. There is no
Ourios-side compensation for a lost acknowledged batch.

## 5. Acceptance criteria

RFC0067.1–.3 check the enacting PR. RFC0067.4–.7 are the behaviour the
rule requires; they are tested by RFC 0066's implementation, and RFC
0066 cannot reach `specified` until its §5 carries them.

> **Scenario RFC0067.1 — `AGENTS.md` §3.4 matches §3.2–§3.3**
> - **Given** the enacting PR
> - **When** `AGENTS.md` is diffed against the version before it
> - **Then** §3.4's opening paragraph and the new bullet are exactly
>   §3.2's text, the footer carries §3.3's sentence and a new
>   `Last updated` date, and nothing else in the file changes

> **Scenario RFC0067.2 — `docs/hazards.md` matches §3.4**
> - **Given** the enacting PR
> - **When** `docs/hazards.md` is diffed against the version before it
> - **Then** H3's new bullet and escalation line and H8's banner read as
>   §3.4 gives them, and no other hazard changes

> **Scenario RFC0067.3 — citations still resolve**
> - **Given** the enacted `AGENTS.md`
> - **When** `just agents-citations` and `mdbook build` run
> - **Then** both pass

> **Scenario RFC0067.4 — an unverified broker without the opt-in refuses to start**
> - **Given** a broker backend whose topic passes RFC 0066 §3.1's checks
>   and has `flush.messages` unset and no `write.caching` key
> - **When** the receiver starts without the opt-in
> - **Then** startup fails before producing or consuming, with an error
>   naming the opt-in field and the two topic keys that would verify
>   persistence

> **Scenario RFC0067.5 — the opt-in starts with a warning**
> - **Given** the topic of RFC0067.4, every partition's replicas on
>   distinct non-empty racks, and the opt-in set
> - **When** the receiver starts
> - **Then** it starts, logs one WARN event naming the topic and stating
>   that acknowledged batches are not verified on disk, and reports the
>   unverified mode in its telemetry

> **Scenario RFC0067.6 — the opt-in still needs separate failure domains**
> - **Given** the opt-in set, and a topic where one partition has two
>   replicas on brokers with the same rack, or a replica on a broker
>   with no rack
> - **When** the receiver starts
> - **Then** startup fails naming the partition and the rack

> **Scenario RFC0067.7 — a verified broker needs no opt-in**
> - **Given** a topic with `write.caching=false`, and separately one
>   with `flush.messages=1`, both passing RFC 0066 §3.1's checks
> - **When** the receiver starts without the opt-in
> - **Then** it starts with no warning and reports the verified mode

## 6. Testing strategy

- RFC0067.1, .2: maintainer review of the enacting PR's diff against
  §3.2–§3.4; the text is fixed here, so the review is a comparison.
- RFC0067.3: the existing `book` CI job.
- RFC0067.4–.7: unit tests in RFC 0066's broker backend against the
  in-memory `BrokerClient` (RFC 0066 §3.2), which returns the topic
  configuration and broker racks each scenario names. No container
  needed.
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
  zone?** *Recommendation: yes, checked at startup from the brokers'
  rack labels, refusing a missing label.* Without it, one power event
  in one rack loses data, which is the exact failure the opt-in asks
  the operator to accept as unlikely.
- [ ] **Q3. Two in-sync copies, or three?** RFC 0064 fixed two as the
  minimum for any replicated WAL. *Recommendation: keep two in the rule
  and recommend three replicas with `min.insync.replicas=2` in the
  operator docs and Helm defaults.* Two survives a disk loss; three
  also keeps accepting writes while one broker is down for maintenance,
  which is an availability choice, not a durability floor.
- [ ] **Q4. On Apache Kafka, does `flush.messages=1` count as "writes to
  disk first"?** The code path fsyncs each replica before the ack, but
  the documentation does not promise it. *Recommendation: yes,* with a
  CI container test against Kafka asserting the ordering through the
  broker's flush metrics, so an upstream change is caught.
- [ ] **Q5. With the opt-in on Apache Kafka, require Eligible Leader
  Replicas?** Without ELR, a single machine power loss can lose
  acknowledged data when it was the last in-sync copy (§3.1, row 3),
  which RFC 0064's "a replica becomes leader only if it already holds
  every acknowledged batch" forbids. ELR is on by default only on new
  clusters from Kafka 4.1. *Recommendation: yes,* checked from the
  broker's finalized features at startup; Redpanda and WarpStream do not
  use Kafka's ISR election, so the check applies to Apache Kafka only.

## 8. References

- `AGENTS.md` §3.4, §3.6, §4, §8.5, footer
- `docs/hazards.md` H3, H8, §Adding a new hazard
- RFC 0012, RFC 0057, RFC 0064 (`meta:` precedent; RFC 0064 §3.1 is the
  base text), RFC 0065 (replicated WAL), RFC 0066 §3.1, §3.2, §3.4,
  §3.12 (Kafka backend)
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
