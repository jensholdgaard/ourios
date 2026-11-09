---
rfc: 0065
title: Replicated WAL and a queryable tail
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-11-01
supersedes: —
superseded-by: —
---

# RFC 0065 — Replicated WAL and a queryable tail

> **Status: `red`.** Every §5 scenario arm has an `#[ignore]`d `todo!`
> stub naming the slice that discharges it: the in-process cluster arms
> in `ourios-ingester` (`tests/it/rfc0065_replicated_wal.rs`), and the
> config, served-binary and chart arms (RFC0065.1, .3, .11's latency
> arm, .14's `require_complete` arms, .17) in `ourios-server`
> (`tests/it/rfc0065_replicated_wal.rs`).
> Specified 2026-11-02 (#1146). Tracks #1141 (option 2). It depends on
> **RFC 0064** (`meta:`, `accepted` 2026-11-02, enacted 2026-11-03),
> which states in `AGENTS.md` §3.4 what a replicated WAL must
> guarantee. §3 states the required behaviour without
> naming a mechanism; §4 surveys the Rust crates that could provide it;
> §6 is the design that follows. The design **extends RFC 0008 §6.2.2**
> (`accepted`) with a new frame kind in the range that section reserves,
> **amends RFC 0005 §3.2** (`accepted`) with three optional columns
> (§6.6), **amends RFC 0009 §3.5** (`accepted`): its "Total row count
> and per-`template_id` counts are invariant across a compaction
> (RFC0009.2)" gains "except rows that repeat an ingest position, of
> which compaction keeps one" (§6.6), **extends RFC 0016 §3.4 and RFC
> 0027's tool results** (`accepted`) with additive `tail_unavailable`
> and `parquet_unavailable` response fields and an additive
> `require_complete` request field (§6.7), adds a group variant to the miner snapshot's
> high-water mark (RFC 0001 §6.9, `accepted`). It reuses RFC 0052's barrier and
> checkpoint (`green`), RFC 0053's retryable rejection (`drafted`) and
> RFC 0059's template-id allocation (`green`), whose reservations §6.7
> moves into the log and scopes per group (coordinate with RFC 0059).
> It coordinates with RFC 0061 (`drafted`, PR #1135), whose request-size
> limit bounds what `Forward` carries. RFC 0066 (the Kafka backend) is a
> second backend that must meet the same §3.

## 1. Summary

Two things are missing today. An acked record is invisible to queries
until its partition is flushed to Parquet, and an acked record not yet
in Parquet is lost with the receiver's disk. This RFC adds a replicated
WAL backend (`receiver.wal.backend: replicated`) that fixes both inside
the one Ourios binary: receivers replicate their WAL among themselves,
each fsyncing its own copy, and replicas that take no part in
acknowledging writes serve the mined-but-unflushed rows to the querier,
which merges them with Parquet and counts every record once. In one
sentence, the acceptance bar is: **the WAL is replicated, it is
queryable, querying it never slows ingest, and query capacity scales
independently of ingest capacity.** §3 states that precisely; §6
proposes how. The local backend stays the default and does not
change.

## 2. Motivation

### 2.1 Fresh queries

The querier reads Parquet only, so an acked record stays invisible until
its partition flushes: up to five and a half minutes on a quiet tenant
at the defaults (RFC 0014 §3.2, #1141), less with RFC 0063's shorter
flush age, which trades freshness for smaller files. Incident queries
("what did this service log in the last minute") want seconds,
independent of how files are cut. That means querying records that are
durable but not yet in Parquet.

### 2.2 Losing a disk loses acked data

A receiver's WAL is one disk. An fsync survives a crash and a power
loss, but losing the disk or the node loses every acked record not yet
in Parquet. Replicating the WAL to other nodes, each fsyncing its own
copy, removes that window without weakening the fsync rule (RFC 0064
§2.2).

### 2.3 Why together

Serving unflushed records from one receiver's memory is fragile on its
own: the records die with that receiver, and the querier has to find it.
Once the WAL is replicated, the records survive the loss of a replica,
and the replication layer already knows which replica owns a tenant.
Each half makes the other cheaper.

### 2.4 One binary, no new dependency

Operators who want replication today would have to run a separate
replicated log. RFC 0066 offers that for teams that already run Kafka.
This RFC is for everyone else: the only shared system is the object
store every deployment already has (`AGENTS.md` §3.6).

### 2.5 One miner per tenant

RFC 0059 §3.6 notes that with `receiver.replicas > 1` every replica
mines every tenant it receives, so one tenant's templates are split
across several miners. Routing each tenant to one replica at a time
gives it one miner.

## 3. Required behaviour

This section names no mechanism. Any design, hand-written or built on a
crate (§4), must meet all of it, and §5's criteria test it. The four
headline requirements are durability (§3.1–§3.3), a queryable tail
(§3.5), isolation of that tail from ingest and independent scaling
(§3.6); the rest makes them precise.

### 3.1 Durability

- **B1.** An export is acknowledged only after its records are fsynced
  on a **write quorum** of the group's voting replicas, each an RFC 0008
  WAL or an equivalent fsynced log. With `openraft` (§4.3) the write
  quorum is a majority of `replication_factor` (§3.8): 2 of 3, 3 of 5.
  No in-memory acknowledgement, ever.
- **B2.** Losing any `write quorum − 1` voting replicas' disks loses no
  acknowledged record: one disk of 3, two of 5.
- **B3.** When fewer than a write quorum can take a write, exports for
  the affected tenants get the retryable rejection (RFC 0053 §3.1: gRPC
  `UNAVAILABLE`, HTTP `503`). The ack never degrades. A rejected write's
  outcome is uncertain, as a lost ack's is today: its entry may still
  commit once a quorum returns, and becomes visible then.
- **B4.** A replica whose local log fails its integrity checks does not
  serve or recover from that log; it rebuilds from a healthy replica.

### 3.2 One writer per tenant

- **B5.** At any time at most one replica can commit writes for a
  tenant. A replica that has lost that role can never commit again
  under it, whatever its clock says. A success it sends late is for a
  record it committed before losing the role, which the new holder has
  (B6), so a late success never confirms a record that can be lost.
- **B6.** A replica takes the role only if it holds every acknowledged
  record of that tenant's log.

### 3.3 Failure behaviour

- **B7.** When the replica holding the role fails, another takes over
  without an operator, as long as B6 can be met. Until then, the
  affected tenants' exports get the retryable rejection.
- **B8.** If no replica can meet B6, the tenants stay unavailable rather
  than lose acknowledged data. Overriding that is an explicit, offline
  operator action, `ourios wal force-leader`, which starts above every
  position the group has ever published, not only above its checkpoint,
  so no position is reused (B13), and reports the range of acknowledged
  records that may be lost before it acts (§4.5, §6.5).
- **B9.** A client never has to know which replica holds a tenant: an
  export sent to any receiver reaches the right one.
- **B10.** An outage of the object store must not stop acknowledgements
  on a healthy group. It may stop failover, membership changes and
  checkpoints.
- **B25. A backend absorbs long stalls.** Ourios is where the pipeline
  ends, not a stage in it, so it absorbs far more backlog than a
  Collector queue: when Parquet cannot be written (an object-store
  outage, the flushing replica down), it keeps acknowledging until its
  WAL volume is nearly full, and only then answers with the retryable
  rejection (B3). The limit is set in bytes (RFC 0053's local bound);
  the chart sizes the volume for **24 hours** at the operator's stated
  ingest rate, and the limit defaults to 90 % of the volume. Ourios
  alerts at 50 % and 80 % of it.
- **B26. A backlog clears.** Mining and flushing run at least twice the
  ingest rate the volume was sized for, so a backlog drains in no longer
  than it took to build, while ingest continues.

### 3.4 Ordering, tenancy and dedup

- **B11.** Each tenant's records are mined in the order of their log
  position, which is the order they were committed; an export
  acknowledged before another was sent always has the lower position.
  Every replica that mines them produces the same rows. Successes of
  concurrent exports may reach their clients in either order.
- **B12.** Tenancy holds everywhere (`AGENTS.md` §3.7): every stored
  frame and every served row carries its tenant, and nothing is ever
  routed or filtered by a shared container alone.
- **B13.** Every record has a position assigned once by the log. A
  record replayed after a failover or redelivered keeps its position,
  and dedup is keyed on it, never on a time window (H8).

### 3.5 The queryable tail

- **B14. Read your acks.** A query that starts after an export was
  acknowledged returns that export's records, whether they are in
  Parquet yet or not, unless the response reports the tenant's tail as
  unavailable (B17).
- **B15. Fresh.** The time from acknowledgement to queryable does not
  depend on the flush age; the target is under a second.
- **B16. Counted once.** A record is returned exactly once while it
  moves from the tail into Parquet, across a flush, a checkpoint, a
  failover and a compaction.
- **B17. Never silently partial.** If a tenant's tail cannot be read,
  the query by default returns its Parquet results and names the tail
  as unavailable in the response; a caller that asks for a complete
  answer (an alerting rule) gets a failed query instead. It never
  returns fewer records without saying so.
- **B18. Same rules as Parquet.** Tail rows go through the same plan:
  tenant and conversation predicates, row masking (RFC 0047) and the
  DSL (H6) apply unchanged.
- **B19. The whole backlog is searchable, at Parquet's cost.** However
  long Parquet writing has been stalled (B25), the tail covers every
  acknowledged record not yet in the object store, and a query's cost
  grows with the time range and selectivity it asks for, as on
  Parquet, not with the size of the backlog.

### 3.6 Isolation from ingest and scaling

- **B23. Tail queries never slow ingest.** A tail read does no work on
  a replica that counts toward acknowledging writes, beyond one small,
  fixed-cost confirmation per batch of reads. A slow, failed or
  overloaded querier or tail replica never delays an acknowledgement.
- **B24. Query capacity scales on its own.** Adding replicas that serve
  the tail raises tail query capacity without changing the write
  quorum, ack latency or ingest throughput. Ingest capacity scales
  separately, through groups and nodes.

### 3.7 Deployment

- **B20.** One binary on every node, configured with its peers; no
  coordination service other than the object store; the same behaviour
  on Kubernetes and off it.
- **B21.** Peer traffic carries authenticated tenant data, so peers
  authenticate each other (mutual TLS) and the peer port is not public.
- **B22.** Mixed versions during a rolling upgrade either interoperate
  or refuse each other; they never corrupt a log.

### 3.8 Configuration surface

```yaml
receiver:
  wal_root: /var/lib/ourios/wal   # every backend that keeps a local WAL
  wal:
    backend: replicated           # local (default) | replicated | kafka (RFC 0066)
    replicated:
      node_id: "${env:OURIOS_NODE_ID}"          # stable and unique, e.g. the pod name
      listen_addr: 0.0.0.0:4320                 # the peer listener
      peers: "${env:OURIOS_RECEIVER_WAL_PEERS}"           # voting replicas: id=host:port,…
      tail_peers: "${env:OURIOS_RECEIVER_WAL_TAIL_PEERS}" # tail pool: id=host:port,… (B24)
      groups: 16
      replication_factor: 3                     # voters per group: 3 or 5
      tail_replicas: 1                          # learners per group from the tail pool, ≥ 1
      tail_clients: "${env:OURIOS_RECEIVER_WAL_TAIL_CLIENTS}" # querier certificate identities allowed to call Tail
      tls: { ... }                              # peer mTLS; RFC 0030's TlsSection leaves
```

Every leaf is `${env:VAR}`-substitutable like `ReceiverSection`, with
bare env under `OURIOS_RECEIVER_WAL_*`. A node is a voter if its
`node_id` is in `peers` and a tail replica if it is in `tail_peers`,
never both, so no id appears in both lists; each list is a
comma-separated `id=host:port` string, so one `${env:…}` carries it.
The peer port defaults to 4320, beside OTLP's 4317 and 4318 and the
querier's 4319. `replication_factor` is 3 or 5 and at most `peers`; its
majority is the write quorum (B1), so the durability floor is derived,
not set (an even factor would add a copy without raising the floor, and
1 is the local backend). `tail_replicas` is `1..=tail_peers`: one tail
replica per group is its flusher (§6.5), so a group without one would
never publish to Parquet. Timing knobs depend on the design and are
listed in §6. Classification under RFC 0004: `backend` is a deployment
choice; `groups` and `replication_factor` are layout and durability
choices fixed at first start (§6.1); `tail_peers`, `tail_replicas` and
`tail_clients` may change at any time (§6.1); the rest are locations
and credentials. `tail_clients` is a comma-separated list of
certificate identities and must be non-empty, since without it no
querier could read the tail (§6.8).

## 4. Background — the Rust replication ecosystem

Surveyed 2026-11-01 against §3, in the style of RFC 0008 §4. Versions
are from crates.io; production users are as the projects report them.

### 4.1 What we surveyed

- **`openraft`** (databendlabs; MIT OR Apache-2.0). Stable 0.9.25
  (2026-07-28, bug fixes only) and 0.10.0-alpha.37 (2026-10-10); "before
  1.0.0, an upgrade may contain incompatible changes". Very active, but
  one author has written about 2400 of its commits (the next, about
  180). Used by Databend's meta-service, CnosDB, RobustMQ and others.
  - *Fit.* Storage is ours: `RaftLogStorage::append` takes an
    `IOFlushed` callback, so one batch-window fsync can complete many
    appends over RFC 0008 segments. The transport is ours (a tonic
    example exists), so RFC 0030's mTLS applies. `openraft-multi`
    shares connections across groups, on 0.10 only. Reads:
    `ReadIndex` (no clock dependence), `LeaseRead` (assumes bounded
    drift) and a documented follower-read flow. Membership: joint
    consensus plus learners. Logs are purged only below a snapshot.
  - *Testing.* A project-run Jepsen suite (partitions, pauses, kills,
    clock skew, membership churn) on every push, and a `turmoil`
    simulation that checks the Raft paper's and TLA+ spec's invariants
    each tick, recently extended to delayed persistence and crash loss.
    Neither is an independent Jepsen analysis.
  - *Bugs.* That testing found at least six safety bugs in 2026, all
    fixed: commit without a real quorum (#1802), a stale membership after
    snapshot install losing committed entries (#1808), Leader
    Completeness in one mode (#1722), a follower log corrupted by a
    leader read error (#1795), a log id reissued after a crash (#2167),
    and a stale replication stream (#2245, 2026-10-31); and a false
    `ReadIndex` confirmation (fixed 2026-11-01). 0.9.25 backports #1802
    and #1808; whether the later ones affect 0.9 is unverified.
  - One `unsafe` block (a `repr(transparent)` cast); modest
    dependencies, most already in our lockfile.
- **`raft`** (tikv/raft-rs; Apache-2.0). Last release 0.7.0
  (2023-03-07); TiKV builds it from git master. `raft-proto 0.7.0`
  requires `protobuf` 2, flagged by RUSTSEC-2024-0437, and a second
  `prost`. A synchronous `Ready` loop leaves the ordering of persist,
  send and apply to the application. No multi-group support; logging
  through `slog`; an open phantom-acknowledgement bug with batched
  appends (#599).
- **`omnipaxos`** 0.2.4 (Apache-2.0). A strong academic design
  (EuroSys 2023), self-described as "in-development", one main
  maintainer, no production users found, and no built-in linearizable
  read found.
- **Not viable:** `async-raft` (dead since 2021, forked as `openraft`),
  `little_raft`, `raftify`, `riteraft`.
- **No crate** implements a log coordinated through object storage, as
  §6 drafts. SlateDB (an LSM store, not a log) uses the same fencing
  primitive, a writer epoch with compare-and-swap on a manifest object,
  which supports §6.2's approach to fencing but offers nothing to adopt.
- **Verification tools.** `turmoil` 0.7.2 (tokio-rs, active) simulates
  the network, and `turmoil-fs` 0.1.0 (2026-10-19) models writes that
  are lost on a crash unless fsynced, which is exactly B1's fault.
  `madsim` swaps tokio and tonic across the workspace and is more
  invasive. `stateright` 0.31.0 has had no commits since 2025-07.

### 4.2 Comparison

| | `openraft` | `raft` | `omnipaxos` | §6 (hand-written) |
|---|---|---|---|---|
| Release | 0.9.25 / 0.10 alpha | 0.7.0 (2023) | 0.2.4 | — |
| Maintenance | very active, one main author | slow, git master | one maintainer | us |
| Published testing | own Jepsen + turmoil | etcd-port tests | Maelstrom | none yet |
| Ack rule | fsync on a majority | majority | majority | fsync on all in-sync |
| Replicas to survive one loss | 3 (acks on the fastest 2) | 3 | 3 | 2, with the store as tiebreaker |
| Depends on the store for elections | no (B10 holds) | no | no | yes |
| Multi-group | `openraft-multi` (0.10) | none | none | per-group record |
| Linearizable read | ReadIndex, follower read | ReadIndex | not found | §6.7's read-index |
| Membership change | joint consensus, learners | joint consensus | reconfiguration | not designed (§9) |

### 4.3 Recommendation

**Adopt `openraft` for per-group replication, behind the `Journal`
seam, with our storage, transport and checkpoint. Do not hand-write the
replication protocol.** §6's riskiest parts (log repair and truncation,
catch-up from a checkpoint, shrinking the in-sync set before an ack, a
former leader's restart) are the bug classes `openraft`'s rigs kept
finding this year, and Kafka needed KIP-101 to fix the truncation class
in its in-sync design. A new protocol would start with none of that
testing and without membership change.

The shape, replacing §6.2–§6.4:
- **Storage:** `RaftLogStorage` over RFC 0008 segments; frame kind
  `0x04` (`ReplicatedEntry`) carries the Raft log id `(term, index)`
  where §6.3 has `(epoch, seq)`, as a tagged record: a blank, normal,
  membership or control entry (§6.5), or a truncation, purge or commit
  marker. The log id
  is `(term, index)` because the type config uses `openraft`'s
  standard leader id (`impls::leader_id_std::LeaderId`); its default
  would add the node id. Votes go in one group-committed vote log per
  node (§4.7, §6.1).
- **Transport:** `RaftNetworkV2` over tonic with RFC 0030 mTLS.
- **Checkpoint:** a metadata-only snapshot `{index, term, Parquet
  manifest reference}`; purging follows it, and a lagging follower
  installs it instead of fetching entries already in Parquet.
- **Tail (B23, B24):** served by non-voting **learners**, which receive
  and apply the committed log but never count toward an ack, so adding
  them scales query capacity without touching the write path. A learner
  confirms it is current with one read-index confirmation from the
  leader per batch of reads, an Ourios peer RPC over openraft's
  follower-read flow (the leader's `get_read_linearizer(ReadIndex)`
  returns the read log id; the learner's `Linearizer` waits until it
  has applied through it), then reads
  locally. Learners mine the raw frames themselves with a deterministic
  miner (§6.7), and one of them, the flusher, writes Parquet (§6.5);
  the leader does not mine.
- **A lost or corrupt disk** (B4) rejoins through a membership change
  (removed, re-added as a learner, promoted), never under the same
  identity: a node that forgot its vote could vote twice in one term.
- **The store** keeps the cluster layout and checkpoints only.

**Costs.** Surviving one loss takes three replicas, not two (one more
disk and fsync per group), though acks wait for the fastest majority,
not the slowest in-sync member. `client_write` resolves after apply, so
apply only queues frames for the miner. Clock-free tail reads cost a
read-index round trip to the leader per read batch.

**Risks.** 0.10 is alpha and still receiving safety fixes, so we would
pin a reviewed git revision; 0.9 is frozen and may lack the latest
fixes; `openraft-multi` exists only on 0.10. The bus factor is about
one, so the `Journal` seam keeps §6 and RFC 0066 as fallbacks. Every
dependency bump re-runs RFC0065.3 and the simulation suite.

### 4.4 Spike result

A throwaway spike (2026-11-01, `openraft` 0.10.0-alpha.37 on the
pinned Rust 1.99.0, over the real `ourios-wal`) answered the three
questions the survey left open. Seven tests passed in three serial
runs and one parallel run:

- **Storage over RFC 0008 segments: pass.** `openraft`'s own storage
  test suite passes. With fsync stalled, 200 append batches were
  readable and none acknowledged; one `Wal::sync` then completed all
  200, so group commit works. Conflict truncation and purge are marker
  frames that replay honours, fsynced before they return, so the WAL
  needs no tail-truncate primitive. The vote lives in its own fsynced
  file (tmp, fsync, rename, directory fsync), because segment
  reclamation could delete it from the WAL; §4.7 found that a file per
  group does not scale and replaces it with one vote log per node.
- **A snapshot at the Parquet checkpoint: pass.** A snapshot at index
  11 with applied at 21 is accepted, applied state does not revert,
  purge stops at the snapshot, and a follower isolated through 20
  writes installs that one snapshot and catches up from index 12, then
  counts toward acks. A leader recovers from power loss with its
  checkpoint and every acked entry.
- **Reads and crashes: pass, on an in-memory network.** Every one of 50
  acks had at least two fsynced copies; a write fsynced only on the
  leader stayed unacknowledged. After a follower lost 350 unsynced
  bytes and the leader was then killed, `ensure_linearizable(ReadIndex)`
  on the new leader and a follower read (the leader's `read_log_id`,
  then `try_await_ready`) both saw every acked entry. A frozen, deposed
  leader committed nothing. `turmoil`'s network was not used (it needs
  a serialising transport) and `turmoil-fs` cannot intercept
  `ourios-wal`'s `std::fs` calls without changing the crate, so crash
  loss was simulated by cutting each segment back to the offset the
  last `Wal::sync` returned.

**Consequences for the implementation:**
- `openraft` is the design; §6.2–§6.4 are kept as the fallback behind
  the same seam (§6's note).
- Configuration: `enable_leader_restore = false` (its default is unsafe
  without a synced commit index) and an explicit pre-vote policy (the
  default grants every request).
- A new leader answers `ForwardToLeader { leader_id: None }` until a
  quorum acknowledges it; that maps to the retryable rejection (B3).
- `install_full_snapshot` panics on a snapshot that conflicts with the
  vote or committed log, so a checkpoint read from the store is
  validated before it is installed.
- The checkpoint is replicated as a log entry, so every replica, not
  only the leader, can snapshot and purge.
- `ourios-wal` gains frame kind `0x04` carrying the tenant (RFC 0052's
  ledger and retain floors key on it), a read-by-offset API (today
  only a full replay exists), and `sync` on a dedicated thread.
- Dependencies: 17 crates not yet in the lockfile.
- The simulation harness (§8) is the in-memory network with fault
  injection and the segment-cut crash model; `turmoil` and an
  `ourios-wal` filesystem shim are optional later work.

### 4.5 Operations spike result

A second spike (2026-11-01; 8 tests on 0.10.0-alpha.37 and 6 on 0.9.25,
each green in repeated runs) covered membership, disk loss, forced
takeover and the version choice.

- **Membership: pass.** Under continuous writes, a new learner caught up
  by installing the checkpoint snapshot, was promoted, and the leader
  then removed itself; acks never stopped (the longest gap, about 1.1 s,
  was the leader handoff) and every acked entry was on every remaining
  voter. `add_learner` returns once the learner is within
  `replication_lag_threshold`, not once it has caught up, so promotion
  waits for the learner's applied index itself.
- **A wiped disk under the same identity loses acked data.** With the
  leader alive, `openraft` detects the reverted follower only by
  panicking the leader's core (the next write fails; metrics still read
  healthy). With the leader lost, it does not detect it at all: the
  wiped node, having forgotten its vote, voted for a stale node, which
  became leader, and **acked entries were lost**. Removing the lost node
  and adding a **new, never-used id** as a learner, then promoting it,
  lost nothing; so did the same path for a log that fails its CRC check
  (the node refuses to start). On Kubernetes this is exactly what a
  StatefulSet pod rescheduled onto a fresh volume does, so §6.1 makes a
  replica's identity live on its disk, not in its pod name.
- **Forced takeover: possible only by offline surgery.** With two of
  three voters gone, no API lets the survivor lead alone. Editing its
  disk offline (a membership entry naming only itself at the next index
  in a higher term, plus a matching vote) works. Done naively, the first
  new write reused a position at or below the store's checkpoint,
  breaking B13; starting from the store's checkpoint (install it as the
  snapshot and purge to it first) did not. What is lost is exactly the
  acknowledged entries above the checkpoint that only the dead replicas
  held.
- **Version: 0.10, at a pinned revision.** 0.9.25 passes storage, the
  snapshot below applied, ReadIndex, follower reads and fencing, but it
  always restores a restarted leader with no setting to stop it, and
  with our commit index not fsynced a ReadIndex on the restored leader
  answered below the last ack in 10 of 10 rounds (B14). Fsyncing the
  commit index fixed it at one more fsync per commit. On 0.10 with
  `enable_leader_restore = false`, 0 of 5 rounds were stale (5 of 5 with
  the default). The 2026 fixes for #2245 and the false ReadIndex
  confirmation are on `main` only, newer than 0.10.0-alpha.37, so the
  pin is a `main` revision that contains both (commits 9567ffea and
  0240eca5); 0.9.25 is unaffected by them but has about nine later
  fixes not backported.

**Settings this fixes** (0.10, `src/config/config.rs`):

| Setting | Value | Why |
|---|---|---|
| `enable_leader_restore` | `false` | the default `true` served stale reads in the spike |
| `allow_log_reversion` | `false` (default) | enabling it hides the wiped-disk case above |
| `snapshot_policy` | `Never` | snapshots follow Parquet checkpoints only |
| `max_in_snapshot_log_to_keep` | small | entries below the checkpoint are in Parquet |
| `enable_pre_vote` | `true`, with `pre_vote` implemented | the trait's default grants every request; without it a partitioned survivor kept raising its term |
| `install_snapshot_timeout` | raised | if a checkpoint install ever reads the store |
| election and heartbeat timeouts | defaults, then tuned (§6.9) | liveness only |

`cluster_name` is a tracing label, not a guard: the transport routes by
group id so no RPC crosses into another group.

### 4.6 Performance spike result

Indicative only: one laptop (10 cores, SSD), every node in one process
over an in-memory network, with antivirus scanning the writes and
another spike running; means were stable across repeats, p99s noisy.
`openraft` 0.10.0-alpha.37, release build, fsync off the core task.

- **Ack latency.** With fast disks, a 3-voter group adds about
  0.3–0.4 ms at p50 over the local WAL (1 KiB at 500/s: 0.17 ms local,
  0.40 ms Raft); 5 voters cost barely more. With a modelled disk per
  node (about 3.5 ms per fsync) the leader's and a follower's fsyncs
  overlap, so Raft adds about 0.25 ms to a 4.5 ms ack. Closed-loop
  throughput on one shared disk was 3–4× below local, from the leader's
  single core task and three times the bytes on one disk; separate disks
  and the pipelined `stream_append` transport (measured faster than
  sequential) are what production has.
- **Many groups.** `openraft`'s own idle cost is linear, about 4 ms of
  CPU per group per second (50 ms heartbeats) and under 1 MiB per
  group; 512 groups cost about 2 cores of heartbeats per process. The
  real knee is ours: each RFC 0008 WAL preallocates an 8.3 MiB RFC 0052
  reclaim sidecar sized for 1 024 tenants, so 512 per-group WALs would
  cost about 4.3 GiB of disk and memory per node before any data. A
  128-group run with WALs did not finish; §4.7 diagnoses it.
  `openraft-multi` only shares connections; it changes none of this.
- **Tail isolation (B23, B24).** Learners never sit in the ack path:
  with tail reads at saturation, ingest held its rate and ack p99
  stayed within the noise of the run without reads. Each ReadIndex
  confirmation costs the leader about two append messages; batching one
  confirmation across all queued reads cut confirmations 4.5× and the
  leader's messages 2.8×, and `openraft`'s piggyback option (wait for
  the next heartbeat) adds none at about 1.7 ms per read. A second
  learner doubled tail reads (55 000 to 100 000 a second) with voters,
  quorum and ack p50 unchanged.
- **Failover.** The time from killing the leader to the first ack on a
  new one is about 1.8 × `election_timeout_max` (the vote is refused
  until the old leader's lease, equal to that maximum, expires): about
  0.55 s at `openraft`'s defaults (50 ms heartbeat, 150–300 ms
  election), 0.22 s at 20 / 60–120 ms. The tail is unavailable for the
  same window and back within about 2 ms of the first ack.

**Consequences:** §6.1 sizes each group's WAL sidecar to the group
instead of the node (coordinate with RFC 0052, whose sidecar geometry
becomes a WAL setting); §6.7 batches read confirmations; §6.9 starts
from `openraft`'s timeouts with the failover bound stated.

### 4.7 Many-groups stall diagnosis

The 128-group run in §4.6 that never finished was reproduced on the
same laptop (2026-11-01) and diagnosed. It is ours, not `openraft`'s,
and it changes the storage design.

- **Cause: an election storm from blocking vote writes.** Persisting a
  vote (§4.4's tmp file, fsync, rename and directory fsync) took about
  17 ms and ran on the Raft runtime's worker threads. With 384 replicas
  electing at once, four workers spent their time in `open` and
  `fsync`, heartbeats and vote replies missed the 150–300 ms election
  timeout, and elections restarted: about 190 vote requests a second
  for 90 s with almost no appends and no group ever settling. A stack
  sample showed the workers inside the vote write. 16 groups (48
  replicas) stayed under the threshold, which is why §4.6's other runs
  passed.
- **Moving the write off the workers is not enough.** Run on the
  blocking pool, 128 groups elected in about 6 s with plain `fsync`,
  but each vote then waited about 0.3 s behind the others on the disk,
  and with a drive-cache flush (`F_FULLFSYNC`, the durable option on
  macOS) the storm returned: vote writes averaged about 1 s, the
  slowest 14 s, and some group still had no leader after 26 s. A
  control that skipped the fsync converged in under 1 s, confirming the
  disk write as the bottleneck.
- **One vote log per node, group-committed, fixes it.** All groups on a
  node append their votes to one file, and every vote queued while an
  fsync runs goes out in the next one, off the workers. With
  drive-cache flushes, the 128-group cluster elected every leader, and
  killing one of the three nodes (43 groups lost their leader at once)
  gave the first ack on every new leader at 0.73 s median and 0.83 s
  max, with 52 fsyncs for 187 vote writes. At 256 groups (86 at once)
  it was 1.1 s median and 1.6 s max, with 103 fsyncs for 512 writes.
  With plain `fsync`, 128 groups failed over in 0.48 s median, the same
  as one group (§4.6), against 1.7 s with a file per group.
- **The rest of the run's slowness was the sidecar.** Opening 384 WALs
  took 9 s, all of it the 8.3 MiB RFC 0052 sidecar preallocation that
  §6.1 already sizes per group.

**Consequences:** §6.1 keeps votes in one group-committed log per node
and requires that no storage operation block the Raft runtime's
workers; RFC0065.27 tests a node failure with many groups; §6.9 adds
the mass-failover time and vote-log batching as tuning signals.

## 5. Acceptance criteria

> **Scenario RFC0065.1 — the backend is configurable and checked at start**
> - **Given** `receiver.wal.backend` unset, `local`, and `replicated`
>   with every field set, and then `replicated` without `peers`, with a
>   `node_id` in neither `peers` nor `tail_peers`, with
>   `replication_factor` 4 or above `peers`, with `tail_replicas` 0, and
>   without `tls`
> - **When** the server resolves its config and starts
> - **Then** the first three start (local, local, replicated), and each
>   of the rest fails startup with an error naming the field
> - **And** a node whose `peers`, `groups` or `replication_factor`
>   differs from the recorded layout fails startup naming the field

> **Scenario RFC0065.2 — an ack waits for the write quorum (B1, B3)**
> - **Given** a group of three voters
> - **When** an export is sent while both followers' fsyncs are held
> - **Then** no success acknowledgement is sent until a second voter has
>   fsynced it
> - **And** with both followers down, the export gets the retryable
>   rejection and no success acknowledgement is ever sent for it; if
>   its entry commits after the followers return, its records become
>   visible then (B3's uncertain outcome)

> **Scenario RFC0065.3 — crash recovery on the replicated backend (B2, B7)**
> - **Given** a three-node cluster ingesting a steady stream
> - **When** the replica acknowledging a group is killed with SIGKILL
>   mid-batch
> - **Then** another replica takes over without an operator, every
>   acked record is in Parquet after the next checkpoint, and after
>   compaction no `(tenant, ingest position)` appears twice

> **Scenario RFC0065.4 — a replaced writer cannot acknowledge (B5)**
> - **Given** the acknowledging replica paused while another takes over
> - **When** it resumes and processes its queued turns, with its clock
>   set arbitrarily
> - **Then** it commits nothing new: every success it sends is for a
>   record the new writer holds, and its other waiting clients get the
>   retryable rejection

> **Scenario RFC0065.5 — takeover only by a replica with every ack (B6, B8)**
> - **Given** a group in which only replicas `a` and `b` hold every
>   acked record, and a lagging replica `c`
> - **When** `a` and `b` are both down
> - **Then** `c` does not take over, the group's exports get the
>   retryable rejection, and when `b` returns it takes over with every
>   acked record

> **Scenario RFC0065.6 — losing and regaining a follower (B1, B3)**
> - **Given** a group of three voters
> - **When** one follower stops, then returns
> - **Then** acks continue with two replicas, the returning follower
>   catches up before it counts toward an ack again, and no ack ever
>   counted a replica that had not fsynced the record

> **Scenario RFC0065.7 — a diverged follower is repaired (B6)**
> - **Given** a follower holding entries an earlier writer appended but
>   never committed
> - **When** a new writer takes over
> - **Then** the follower's log converges to the writer's, and no
>   committed entry is removed

> **Scenario RFC0065.8 — routing and tenancy (B9, B11, B12)**
> - **Given** the tenant and placement hashes, and two tenants in one
>   group
> - **When** they run on a fixed set of tenant ids and peers, and the
>   two tenants export to a node that does not hold their group
> - **Then** the hashes return the golden values committed with this
>   RFC's implementation, both exports are acked and mined in log
>   position order, an export acked before the other was sent mined
>   first, and each tenant's query returns only its own rows
> - **And** a forwarded export whose leader is not the node the sender
>   tried is retried at the hinted leader and acked once, while one
>   whose forwarded call times out or loses its answer gets the
>   retryable rejection and is not sent again

> **Scenario RFC0065.9 — a corrupt replica does not recover from itself (B4)**
> - **Given** a follower whose group log fails an integrity check
> - **When** it restarts
> - **Then** it serves nothing from that log, rebuilds it from a healthy
>   replica, and no record is lost or duplicated

> **Scenario RFC0065.10 — the protocol holds its invariants under faults**
> - **Given** the in-process cluster (§8) with message loss,
>   reordering, partitions, node crashes, segment-cut disk loss and
>   arbitrary clock skew, driving writes, membership changes, flusher
>   handoffs, checkpoints and tail reads
> - **When** the deterministic simulation suite runs at least 1 000
>   seeds in CI (and every failing seed is kept as a regression test)
> - **Then** no run breaks B1, B5, B6, B14 or B16
>   (B1, B5 and B6 are checked against `openraft`'s quorum commit, the
>   design §4.3 picks; RFC0065.2, .4, .5 and .7 test the same design)
> - **And**, only if the §6.2–§6.4 fallback is ever built to replace
>   `openraft`, a model of the whole fallback protocol is model-checked
>   to a bounded depth before it ships; while `openraft` is the design
>   this arm has nothing to check and its stub stays ignored

> **Scenario RFC0065.11 — a record is queryable within the allowance (B14, B15)**
> - **Given** a replicated backend with the flush age at 300 s
> - **When** an export is acked and a query for it starts afterwards
> - **Then** the query returns its records with no flush having run
> - **And** on the CI runner, the time from ack to a query returning the
>   record is under 5 s (an allowance over the sub-second target)

> **Scenario RFC0065.12 — the tail and Parquet count a record once (B16, B19)**
> - **Given** a stream of acked records and a test clock that controls
>   flushes, checkpoints, failovers and compaction
> - **When** the same `count` query runs at random points between them
> - **Then** every run returns the number of records acked before it
>   started (a `proptest` over the event order)

> **Scenario RFC0065.13 — the tail keeps tenancy and visibility (B12, B18)**
> - **Given** two tenants in one group, and a masked attribute under
>   RFC 0047
> - **When** each tenant queries records still in the tail
> - **Then** each sees only its own rows, and the masked attribute is
>   masked exactly as in Parquet results
> - **And** a `Tail` or `TailScan` call from a client certificate not in
>   `tail_clients`, a voter's or tail replica's own included, gets
>   `PERMISSION_DENIED` and no rows

> **Scenario RFC0065.14 — an unreachable tail is reported (B17)**
> - **Given** a group whose tail cannot be read (its writer down, no
>   takeover yet)
> - **When** a tenant in it is queried
> - **Then** the response carries the Parquet results and names the
>   tail as unavailable; it never returns fewer records without saying
>   so
> - **And** the same query with `require_complete: true` fails with the
>   retryable unavailable error (HTTP `503`, an MCP tool error) naming
>   the group in the §6.7 wire form, and returns no rows; the same holds for
>   `parquet_unavailable` during a store outage (RFC0065.21)

> **Scenario RFC0065.15 — a store outage does not stop acks (B10)**
> - **Given** a healthy three-node cluster
> - **When** the object store becomes unreachable
> - **Then** exports keep being acknowledged, and failover and
>   checkpoints resume once it returns

> **Scenario RFC0065.16 — the local backend is unchanged**
> - **Given** `backend: local`
> - **When** the RFC 0008, RFC 0014 and RFC 0052 suites run
> - **Then** they pass unchanged, and written Parquet files have no
>   `ingest_*` columns

> **Scenario RFC0065.17 — the chart renders the replicated shape (B20, B21)**
> - **Given** `receiver.wal.backend: replicated` and `receiver.replicas: 3`
> - **When** the chart renders
> - **Then** the receiver config carries the three peers on the
>   headless Service and `node_id` from the pod name, the peer port is
>   absent from the public Service, and the peer TLS secret is mounted

> **Scenario RFC0065.18 — tail queries do not slow ingest (B23)**
> - **Given** a three-node cluster with one tail replica per group,
>   ingesting at a fixed rate
> - **When** tail queries run at saturation against the tail replicas,
>   and then a tail replica is paused
>   (each phase runs for the same duration in the same run, after a
>   query-free baseline phase)
> - **Then** the ack latency p99 is within 10 % of the baseline phase's
>   and ingest throughput within 5 % of it (starting bounds, tuned from
>   the spike's run-to-run spread), and no ack waits on the paused tail
>   replica

> **Scenario RFC0065.19 — tail capacity scales on its own (B24)**
> - **Given** a group with one tail replica under saturating tail load
> - **When** a node is added to `tail_peers` and `tail_replicas` raised
>   to 2, with `peers` unchanged
> - **Then** tail query throughput rises, while the voting set, the ack
>   quorum and the ack latency do not change

> **Scenario RFC0065.20 — every tail replica mines the same rows (B11, B16)**
> - **Given** a group whose flusher and another tail replica mine the same
>   committed log, across an id-block reservation, a flusher change, a
>   tail-replica restart from a checkpoint snapshot and a second tenant
>   in the group
> - **When** their rows are compared position by position
> - **Then** every row is identical, `template_id` included

> **Scenario RFC0065.21 — a long object-store outage is absorbed and searchable (B25, B19)**
> - **Given** a three-node cluster sized for a backlog of `N` bytes,
>   ingesting at a fixed rate
> - **When** the object store is unreachable until the backlog reaches
>   half of `N`
> - **Then** every export is acknowledged, a query over the whole outage
>   returns every acknowledged record, a selective query reads a number
>   of bytes that does not grow with the backlog, and the 50 % alert has
>   fired
> - **And** when the backlog reaches the limit, exports get the
>   retryable rejection and nothing already acknowledged is lost

> **Scenario RFC0065.22 — a backlog drains while ingest continues (B26)**
> - **Given** the backlog left by RFC0065.21
> - **When** the object store returns and ingest continues at the same
>   rate
> - **Then** the backlog drains in no longer than it took to build, and
>   a `count` over the window is the same before, during and after

> **Scenario RFC0065.23 — the leader never mines (B23, B11)**
> - **Given** a three-node cluster with two tail replicas per group
> - **When** the flusher is killed mid-flush
> - **Then** the leader's ack latency is unaffected, the other tail
>   replica becomes the flusher without replaying the log, and after
>   compaction no `(tenant, ingest position)` appears twice

> **Scenario RFC0065.24 — a wiped disk rejoins as a new replica (B4, B6, B8)**
> - **Given** a group of three voters, and a voter restarted under the
>   same name on an empty volume, once while the other two (one of them
>   the leader) are up, and once while the leader is also lost
> - **When** the cluster continues
> - **Then** in the first case the node joins under a new identity as a
>   learner and is promoted only once caught up, by one membership
>   change that also removes its old identity, so no membership ever
>   lists two voters with its `node_id` and the old identity is gone
>   after promotion; this holds when the leader changes between the
>   steps; no node's core panics, and every acknowledged record is
>   present
> - **And** in the second case only one original voter remains, so the
>   group fails closed: no election, no membership change and no ack
>   until an original quorum returns, with the retryable rejection for
>   its tenants; permanent loss of that quorum is B8's offline recovery
> - **And** on a fresh cluster, the group forms once from the voters'
>   `Identify` answers, recorded in `formed.json`: when the first-ranked
>   voter records it and then restarts on an empty volume before it
>   initializes, the other two initialize with the recorded membership,
>   elect a leader, and the restarted voter joins under its new identity
>   by the replacement above, never by initializing a second membership;
>   and a `formed.json` that does not parse, is truncated, has another
>   `format` or `group`, or whose `node_id`s are not the group's voters,
>   forms nothing

> **Scenario RFC0065.25 — a forced takeover reports its loss and reuses no position (B8, B13)**
> - **Given** a group whose other two voters are lost for good, with
>   acknowledged records above the store's checkpoint that only they
>   held, some of them already flushed to Parquet by the flusher
> - **When** an operator runs `ourios wal force-leader` on the survivor
> - **Then** it prints the possibly-lost range before acting, the
>   survivor leads alone, and its first new entry's position is above
>   every position in any published file, so no new row collides with
>   an old one; every template id it mints is above the store's id
>   high-water when it acted, so no id in a published file names a
>   different template; and a restored template the lost suffix had
>   already widened, widened again by the survivor, gets a version
>   above every published one for its id

> **Scenario RFC0065.26 — a delayed former flusher cannot move a checkpoint back**
> - **Given** a group whose new flusher has checkpointed `seq` 200 and
>   whose replicas have purged below it
> - **When** the former flusher, delayed, writes its bundle and
>   `checkpoint.json` for `seq` 100 and proposes its checkpoint entry
> - **Then** `checkpoint.json` still names 200, the leader rejects the
>   proposal, and a restarting tail replica restores from 200
> - **And** a flusher killed between any two of its bundle puts, or
>   before its `checkpoint.json` put, leaves `checkpoint.json` as it was
>   and proposes no checkpoint entry, and no `bundle.bin` is present
>   before every `.snap` it names; a bundle object already present with
>   other bytes, or a `checkpoint.json` that does not parse, has another
>   `format`, a missing or unknown field, or a `bundle` that is not its
>   `seq`'s prefix, is never replaced, and that checkpoint fails with
>   nothing purged; a retried put of identical bytes completes the
>   checkpoint; and a new log incarnation
>   over a bucket holding an old one's objects checkpoints under its
>   own log id from its first cut

> **Scenario RFC0065.27 — a node failure with many groups fails over like one group (B7)**
> - **Given** three voter nodes hosting 128 groups, durable fsync on,
>   and every group led and acknowledging
>   (the run first kills the leader of a single group alone and records
>   that failover time, `F`)
> - **When** one node is killed, so every group it led loses its leader
>   at once
> - **Then** every one of those groups acknowledges on a new leader
>   within `2 × F` of the kill, no group is
>   left without a leader, and the node's vote log took fewer fsyncs
>   than vote writes

> **Scenario RFC0065.28 — the tail never resurrects an erased row**
> - **Given** a conversation whose rows the flusher has published above
>   the checkpoint, with the checkpoint not yet past them, and
>   separately a flusher paused after writing a file of that
>   conversation and before its published entry commits
> - **When** an erasure of that conversation completes, the paused
>   flusher then resumes and commits its published entry, and the
>   tenant is queried,
>   with the store reachable and during an outage
> - **Then** no erased row is returned: with the store reachable the
>   rewritten file answers for them, and during the outage that file is
>   named in `parquet_unavailable`; no tail replica serves them (a
>   pre-request row a flusher publishes after the confirming pass is
>   RFC 0009 D3's residual, pinned by RFC0009.22, not this scenario)

> **Scenario RFC0065.29 — a log entry a replica cannot read stops it (B11)**
> - **Given** a group WAL holding a flusher assignment, then a control
>   entry of a kind the replica does not know, or a flusher assignment
>   whose body is shorter or longer than 16 bytes
> - **When** the replica opens the group's log
> - **Then** it reads the first assignment's UUID, and it refuses to
>   open the log at the unreadable entry rather than skip it; a leader
>   whose members have not all reported reading a kind the log has never
>   held does not append it, and one whose log already holds a kind
>   appends it with a member lost; a leader holding an uncommitted entry
>   of a new kind admits no learner until it commits, and a learner it
>   then admits reads that kind, and one with a membership change in
>   flight appends no new kind until that change commits; a replica
>   whose checkpoint records a kind above the highest it reads refuses
>   to open the log at the checkpoint

> **Scenario RFC0065.30 — the leader names one flusher and keeps it (B11)**
> - **Given** a group with three tail replicas as learners and no
>   flusher assignment
> - **When** the leader has led for the flusher patience, and then the
>   flusher stops acknowledging, recovers, leaves the tail placement, or
>   the leader fails over
> - **Then** it assigns the first healthy replica in tail placement
>   order; a flusher unhealthy for the patience is replaced by the next
>   healthy one and does not take the role back on recovery; one that
>   leaves the placement is replaced at once, by a remaining replica that
>   has mined through the commit point, before its learner is removed;
>   with no replica healthy no
>   assignment is appended; and a new leader keeps the flusher its log
>   names for the patience

> **Scenario RFC0065.31 — a checkpoint snapshot holds the group at its cut (B11)**
> - **Given** a group whose log holds a miner configuration, id blocks,
>   a change of `similarity_threshold` after templates were mined, a flusher
>   change and a learner added after the cut of a checkpoint
>   entry, and checkpoint entries proposed by a replica that is not the
>   flusher at their position, or with a cut not above the last applied
>   one
> - **When** every replica applies the log, and a replica whose log is
>   purged below the cut installs the snapshot and catches up
> - **Then** only the flusher's checkpoint with a cut above the last is
>   applied; its snapshot's last log id is the cut and it holds the
>   membership, flusher, highest kind, miner configuration and latest
>   id block as of the cut; the installing
>   replica ends with the same flusher, membership, blocks and
>   configuration as one that applied the whole log, and mines the
>   lines after the configuration change into the same templates as a
>   replica that mined through it; a block below the previous one is
>   ignored, also by a replica whose log is purged below that previous
>   block, and after `(0, 100]` is applied and `(0, 50]` ignored,
>   `(50, 150]` is ignored too; a miner configuration body that `MinerConfig` refuses, or
>   one that changes `prefix_depth` or `max_node_children` from the
>   group's first, stops the replica as RFC0065.29's unreadable entry
>   does, and a leader whose own `prefix_depth` differs appends the
>   log's, also when elected after a checkpoint purged every
>   configuration entry; and an
>   otherwise valid version-1 snapshot record fails the group's open
> - **And** the bundle at the cut holds, for each tenant of the group, a
>   version-3 miner snapshot marked `{log, seq}` with the log id and
>   the cut, and the allocator's range, its unused blocks and each
>   tenant's configuration kind and body in force; a replica restored
>   from it mints the same next template id and the same rows as one
>   that mined through the cut; a bundle whose `format`, CRC, log id,
>   cut, current block and `next` (`next` `0`; latest block `(1000,
>   2000]` with no pending block and `next` `1`; a pending `(0, 2000]`
>   against that latest block with `next` `1`), pending blocks, tenant
>   order, a snapshot's length, CRC, version
>   or mark, or a configuration kind or body is not as §6.5 requires
>   (a `0x04` body whose `similarity_threshold` differs from the
>   record's included), or that
>   misses an object, fails the restore closed rather than replaying,
>   and no restored allocator issues `0`;
>   and a local-backend version-3 snapshot loads and restores as it did
>   before this amendment, while one marked `{log, seq}` is discarded
> - **And** a restarting tail replica restores from the bundle at its
>   snapshot record's cut even when `checkpoint.json` names a later one,
>   and mines only the entries after it; one restarting while the store
>   is unreachable serves no tail and does not flush for the group,
>   reports why, keeps taking the log while the group keeps acking, and
>   restores and mines once the store returns
> - **And** on a store without `miner/template_ids.v1.json` or
>   `log_ids.json`, of two replicated starts only the one whose create
>   of `log_ids.json` succeeds bootstraps the object, by RFC 0059 §3.5,
>   and marks it `seeded` before any log id is allocated; the other,
>   paused before its create and resumed after the first has seeded,
>   allocated log ids and reserved a block and the object was deleted,
>   does not re-create it; a bootstrapper killed between the
>   object and `seeded` finishes on its restart while every other start
>   waits; an unreadable `log_ids.json` fails the start; and with the
>   object deleted after
>   group A has applied `(0, 1000]` and published nothing, while group
>   B has applied no block, neither A's leader nor B's bootstraps or
>   appends a block, and no id A holds is reserved again; nor does a
>   leader while the object's `reserved_through` is below its group's
>   latest block's `through`

## 6. Proposed design

> **`openraft` is the design (§4.3, §4.4).** §6.2–§6.4 below
> (election, replication, fencing, repair) are the hand-written
> fallback, which acknowledges on a configured in-sync floor
> (`min_in_sync`, `2..=replication_factor`, the RFC 0064 minimum) instead of a majority, and
> is kept because the `Journal` seam lets it replace `openraft` if the
> dependency has to go; with `openraft`, §4.3's shape replaces
> them. §6.1 and §6.5–§6.11 apply to both, with Raft's log id
> `(term, index)` in place of `(epoch, seq)` and §6.7's tail replicas
> as `openraft` learners.

### 6.1 Cluster layout

- **The voting layout is recorded** in
  `<bucket>/wal/replicated/cluster.json` (voting peer ids, `groups`,
  `replication_factor`) with `put_if_absent` the first time a node
  starts. A node whose configuration differs refuses to start. Changing
  the voting layout is a migration this RFC does not define; a later
  RFC will (§9, Q5).
- **The tail pool is not part of the layout (B24).** `tail_peers` and
  `tail_replicas` can change at any time. Each group's tail replicas are
  the `tail_replicas` members of `tail_peers` with the highest
  `xxh3_64(group, node_id)`; when that set changes, the group's leader
  adds the new members as `openraft` learners, under the replica UUID
  each answers its `Identify` call with (§6.8; they catch up from the
  checkpoint snapshot), and removes the old ones, a learner-only
  membership change that never touches the voters, the write quorum or
  voter placement. Adding tail capacity is adding tail nodes. The
  current flusher (§6.5) is removed last: when it leaves the set, the
  leader first commits a flusher-assignment entry naming a remaining
  tail replica that has mined through the commit point, and only then
  removes it, so a group is never without a flusher. A tail node that
  restarts on an empty or unreadable `wal_root` answers `Identify` under
  a new UUID with an unchanged `node_id`: the leader adds the new UUID as
  a learner and removes the learner under the old one, which no node
  answers for any more, by the same rule, so a membership never holds
  two learners with one `node_id` for longer than the handoff.
- **A tenant belongs to one group**:
  `group = xxh3_64(tenant_bytes) mod groups`, where `xxh3_64` is
  XXH3's 64-bit variant with seed 0 and `tenant_bytes` are the tenant
  id's bytes exactly as the `TenantOtlpBatch` header carries them (RFC
  0046 §3.3), with no normalisation. The hash, seed and encoding are
  fixed by this RFC and pinned by golden values (RFC0065.8).
- **A group's voters** are the `replication_factor` members of `peers`
  with the highest `xxh3_64(group, node_id)` (rendezvous hashing):
  deterministic, spread across nodes, with no record of its own. Here
  `xxh3_64(group, node_id)` hashes, with seed 0, the group number as a
  4-byte big-endian `u32` followed by the `node_id`'s UTF-8 bytes; equal
  scores rank the smaller `node_id` (bytewise) first. The tail
  placement uses the same score, and both are pinned by golden values
  (RFC0065.8).
- **A replica's identity lives on its disk (B4, B6).** The first start
  on an empty `wal_root` writes a `replica.id` file holding a fresh
  UUIDv7, fsynced, and the Raft node id is that UUID, not `node_id`
  (the pod name stays the address and the label). The mapping lives in
  the group's Raft membership itself: each member's node data is its
  `node_id`, so membership pairs every UUID with the `node_id` it runs
  under, and it is replicated and persisted like any log entry. A node
  whose `wal_root` is empty or unreadable joins as a new replica: it
  answers the leader's `Identify` call with its new UUID under its
  `node_id` (§6.8); the leader, seeing a voter's `node_id` under a UUID
  the membership does not hold, adds it as a learner, waits until its
  applied index reaches the commit point, and then commits **one**
  membership change that
  replaces the voter with the same `node_id` and a different UUID by the
  new UUID (joint consensus, so the voter count never drops). Each step
  is decided from the committed membership alone, so a new leader after
  a failover resumes it; until the replacement commits, the old UUID is
  an absent voter and the group runs on the rest of its quorum. A
  membership never holds two voters with the same `node_id`, and a node
  never rejoins under an identity whose vote it has lost (§4.5).
- **A group's first membership** comes from its voters' own answers and
  is recorded in the bucket, so a group forms once. Only the group's
  first-ranked voter records it, and only while no record exists: it
  calls `Identify` on each of the group's other voters at its configured
  address (§6.8) and, once every one has answered, writes
  `<bucket>/wal/replicated/groups/<group>/formed.json`, the membership
  as each voter's UUID paired with its `node_id`, with
  `put_if_absent`. Every voter whose log for the group is empty reads
  that object, at start and then until it exists, and initializes the
  group with exactly the membership it holds, if its own UUID is in it.
  They all initialize with one membership, so any original majority
  elects a leader even if the recording voter fails before it
  initializes, and a leader replaces a lost voter as above. A voter that
  finds the object naming another UUID for its `node_id`, one restarted
  on a wiped `wal_root`, never initializes: it answers `Identify` under
  its new UUID and joins through that committed replacement, so an
  empty log never forms a second membership beside one that may hold
  acknowledged batches. Until the object exists the group has no
  leader. The object is UTF-8 JSON:

  ```json
  {"format": 1, "group": 3, "members": [
    {"node_id": "ourios-receiver-0", "replica": "0190d2a8-7c1e-7b3a-9f00-5e1c2d3a4b5c"},
    {"node_id": "ourios-receiver-2", "replica": "0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5b"},
    {"node_id": "ourios-receiver-4", "replica": "0190d2a8-a3b7-7d4e-b622-1f2e3d4c5b6a"}]}
  ```

  `format` is `1`; `group` is the group number; `members` holds one
  entry per voter of the group, sorted by `node_id` bytewise, each
  `replica` the lowercase hyphenated UUID that voter answered with. A
  voter initializes from the object only if `format` is `1`, `group` is
  the group it read it for, and the `node_id`s are exactly the group's
  voters under the recorded layout; any other object, or one that does
  not parse, it treats as unreadable: it does not initialize, and
  reports why, so a group never forms from a record it cannot trust.
- **Each replica keeps one RFC 0008 WAL per group** under
  `wal_root/groups/<group>/`, with RFC 0008's segments, fsync policy
  and CRCs unchanged, and its RFC 0052 reclaim sidecar sized for the
  group's share of tenants rather than the node's 1 024, since a
  node-sized sidecar per group costs 8.3 MiB each (§4.6). The sidecar's
  tenant capacity becomes a `WalConfig` setting (coordinate with RFC
  0052).
- **Votes are in one log per node, not per group (§4.7).** Every group
  on a replica appends `(group, vote)` records to
  `wal_root/votes.log`, and one fsync covers every vote queued while
  the previous one ran (group commit); `save_vote` returns after its
  record is fsynced. The log sits outside the group WALs, so segment
  reclamation never touches it; it is rewritten (tmp, fsync, rename,
  directory fsync) holding only each group's latest vote once it
  passes a size threshold. Recovery takes the last record per group.
- **A snapshot is recorded before its purge.** The group's state
  machine writes the snapshot's last log id and membership to
  `wal_root/groups/<group>/snapshot` (tmp, fsync, rename, directory
  fsync) and returns from `install_snapshot` or `build_snapshot` only
  once that is durable, so the purge `openraft` makes after either
  never runs ahead of it. Builds and installs write one at a time,
  and a record newer than the one being written is kept: a build can
  finish after its host was replaced and the new host recorded a
  later snapshot.
  The record carries a format version and a CRC32C. The group's state
  machine opens from it, applied through the snapshot; a record that
  fails either check fails the group's open, and a group without one
  has no snapshot. Format version 2 adds, after the membership, the
  state a checkpoint snapshot holds at its cut (§6.5): the flusher
  (`present: u8 | uuid`), the highest kind (`u8`) and the miner
  configuration in force (`kind: u8`, `0` when none has committed,
  then that kind's body) and the latest id block (`after: u64 |
  through: u64`, the latest applied, both `0` when none has been).
  The data
  `InstallSnapshot` sends is those same bytes, so an installing
  replica gets them with the metadata. Snapshots are built only at a
  checkpoint's cut (the snapshot policy is `Never`; applying a
  checkpoint entry triggers the build), so a snapshot's last log id is
  a cut, and its index the `seq` of the bundle that holds the miner
  state. No deployment runs this backend yet, so version 1 is not
  read: a version-1 record fails the open like an unknown version.
- **No storage operation blocks the Raft runtime's workers.** Vote
  writes, `sync`, truncation and purge markers run on the WAL's own
  threads. An append completes through `openraft`'s `IOFlushed`
  callback once the fsync covering it returns; `save_vote`,
  `truncate_after` and `purge` are async calls whose futures resolve
  after that thread's fsync, so they park only their group's Raft
  task, never a worker; a blocking call on a worker thread caused
  §4.7's election storm. `save_committed` only queues its marker,
  which rides a later fsync: the commit index is not durable on
  return, which is safe only with `enable_leader_restore = false`
  (§4.5).

### 6.2 The group record

Leadership and membership follow PacificA and Vertical Paxos: a
configuration record in a linearizable store decides who leads and who
is in sync, and the data path is primary/backup to the whole in-sync
set. Ourios already relies on the store's compare-and-swap for its
manifests (RFC 0009 §3.4) and the template map (RFC 0033).

Each group has one object, `<bucket>/wal/replicated/groups/<g>/state.json`:

```json
{ "epoch": 7, "leader": "ourios-receiver-1",
  "in_sync": ["ourios-receiver-1", "ourios-receiver-2", "ourios-receiver-0"],
  "lease_until_ms": 1793000000000 }
```

- It changes only by `put_if_match` on the ETag the writer read. A
  write that loses the race re-reads and decides again. Creation is
  `put_if_absent`, with every replica in `in_sync`, since an empty group
  log is identical on all of them.
- **Store support.** Startup fails if the store does not support
  conditional updates (`Store::supports_conditional_update`). The local
  filesystem backend does not, so this design needs S3 or an
  S3-compatible store.
- **Election (B6, B7).** A replica may take leadership only if it is in
  `in_sync` and, by its own clock, `lease_until_ms` has passed. It
  writes `epoch + 1`, itself as `leader`, and a new lease. Every
  acknowledged entry is on every member of `in_sync` (§6.3), so any
  member that wins holds all of them.
- **Lease.** The leader renews `lease_until_ms` every `lease_ms / 3`
  (`lease_ms`, a Tunable, `1000..=60_000`, default 10 000). The lease
  only decides *when* another replica may try to take over; safety does
  not depend on it (§6.4).
- **In-sync changes.** Only the leader edits `in_sync`, and only by
  compare-and-swap. Removing a follower must succeed **before** the
  leader acknowledges anything without it, and is refused when it would
  leave fewer than `min_in_sync` members. A follower returns to
  `in_sync` once it has caught up to the leader's commit point.
- **B10.** Steady-state acks touch only the peers. Lease renewal fails
  during a store outage, so after `lease_ms` no other replica can win an
  election either (the CAS fails), and the leader keeps acknowledging
  as long as its in-sync set is healthy. It cannot shrink `in_sync`
  during the outage, so a follower failure during a store outage stops
  acks (B3).

### 6.3 The write path

- **Routing (B9).** Any receiver accepts an export, authenticates it
  and resolves its tenant exactly as today (auth before decode, RFC 0046
  §3.3). If it does not lead the tenant's group, it forwards the
  `TenantOtlpBatch` payload to the leader over the peer listener and
  answers the client with the leader's answer.
- **The frame.** A new frame kind `ReplicatedBatch = 0x04`, in the range
  RFC 0008 §6.2.2 reserves, carries
  `epoch: u64_le | seq: u64_le | TenantOtlpBatch payload`. `seq` is the
  group's sequence number, dense and assigned by the leader. The header,
  CRC and size cap are RFC 0008's.
- **Append and commit (B1).** The leader runs one RFC 0008 group-commit
  coordinator per group it leads: it appends the turns of one batch
  window, fsyncs, and sends them to every follower in `in_sync` with
  `(epoch, previous epoch and seq)`. A follower checks the epoch
  (§6.4) and that its log ends at the previous `(epoch, seq)`, appends,
  fsyncs, and replies with the highest `seq` it has fsynced. An entry is
  **committed** once every member of `in_sync` has fsynced it, and the
  leader then acknowledges its turns.
- **A slow or failed follower (B3).** If a follower has not replied
  within `replicate_timeout_ms` (a Tunable, `100..=60_000`, default
  2000), the leader removes it from `in_sync` (§6.2) and commits
  without it. If removal would breach `min_in_sync`, the waiting turns
  fail with the retryable rejection and nothing is acknowledged.
- **The trait.** `Journal::append_batch` returns a `JournalPosition`:
  `Local(WalOffset)` on the local backend, `Group { group, seq }` here.
  `sync` returns once every appended entry is committed. The local
  backend's behaviour does not change.
- **With `openraft` (§4.3).** Routing and the trait apply as written;
  the frame, append-and-commit and slow-follower bullets are the
  fallback's. Instead:
  - An export's turns are acknowledged once `client_write` returns
    `Ok`, which `openraft` resolves after a majority of the group's voters
    have fsynced the entry, not necessarily the leader, and the leader
    has applied it (apply only queues the frame, §4.3). With three
    voters that is at least two fsynced copies. `seq` in the position
    is the entry's Raft index.
  - A `client_write` that has not returned within the commit timeout
    fails the waiting turns with the retryable rejection (RFC 0053),
    `error.type` `not_enough_replicas`, and nothing is acknowledged.
    Only the timeout counts as `not_enough_replicas`.
  - An error saying this node does not lead the group means nothing
    was appended, and the export is forwarded (§6.8), as it is from a
    node that does not host the group. Every other `client_write`
    error, such as a stopped group or an entry a leader change
    discarded after appending, fails the waiting turns with the same
    retryable rejection, `error.type` `group_unavailable` (§6.9), and
    nothing is acknowledged. So does a forward that ends without a
    committed position, except for a rejection the leader gives, which
    reaches the client unchanged.
  - After a timeout or an uncertain forward the entry may still commit
    later, the uncertain outcome B3 accepts. The timeout is an internal
    constant starting at 2 s, not a setting; the rate of these
    rejections is the signal that would move it.
  - `wal_batch_window_ms` is only an upper bound. The group log flushes
    as soon as work is queued, so a lightly loaded group fsyncs more
    often and acknowledges sooner; every ack still waits for a
    majority's fsync.

### 6.4 Fencing and log repair

- **Epoch fencing (B5).** Each replica stores the highest epoch it has
  seen per group in `wal_root/groups/<g>/epoch`, fsynced before it acts
  on that epoch, and rejects appends from a lower epoch. A candidate
  fsyncs `epoch + 1` to its own epoch file **before** its election
  compare-and-swap, and a follower's reply carries the epoch it appended
  under. The new leader is a member of `in_sync`, and the old leader
  commits only with every member's fsync at the old epoch, so the
  linearization point is the candidate's epoch fsync: an old-epoch entry
  commits only if the candidate fsynced it before that point, and is
  then in the candidate's log (B6); nothing the old leader appends after
  it can commit. An acknowledgement the old leader sends late is
  therefore always of an entry the new leader holds, and B5's "can
  never commit again" is enforced at the commit, not the send.
  Before the fallback replaces `openraft`, this handshake must pass
  RFC0065.10's bounded model check, including the interleaving of a
  late follower reply with an election. An old leader that tries to remove the
  new one from `in_sync` loses the compare-and-swap, because the
  election changed the ETag. None of this reads a clock.
- **Starting an epoch.** A new leader first raises every reachable
  in-sync member to its epoch, then treats its whole log as committed
  once those members hold it. Entries an old leader appended but never
  committed may be committed this way. They were never acknowledged,
  so the client retried them or will, the same duplicate a lost ack
  makes on the local WAL today.
- **Log repair.** A follower whose log does not end at the leader's
  previous `(epoch, seq)` truncates back to the last entry the two
  share, as Raft's log matching does, then takes the leader's entries.
  Only uncommitted entries are ever truncated. Truncation is a new,
  fsynced operation on the group's WAL, recorded in the epoch file
  before any byte is cut, so a crash mid-repair restarts the repair.
  Kafka's KIP-101 is the cautionary case: truncating by a high-water
  mark instead of by epoch lost committed data.
- **Catch-up.** A returning or lagging follower fetches from any
  in-sync member, starting at its own last `seq`. Entries below the
  group's checkpoint (§6.5) are in Parquet and are not sent; the
  follower starts its log at the checkpoint instead.
- **Corruption (B4).** A replica whose group WAL fails RFC 0008's CRC
  checks leaves `in_sync` (if leader, it stops leading), discards that
  group's log, and catches up from an in-sync member.

### 6.5 Mining, checkpoints and recovery

- **The leader does not mine.** Its work is to acknowledge and
  replicate; mining and flushing never sit on the ack path (B23), and a
  new leader accepts writes as soon as it is elected, without first
  catching a miner up.
- **The flusher mines and writes Parquet (B11).** One tail replica per
  group is the **flusher**, named by a log entry the leader appends (the
  lowest-numbered healthy tail replica; reassigned by another entry when
  it fails). Lowest-numbered is first in the group's tail placement
  order (§6.1); healthy is a learner of the group's committed membership
  whose acknowledgement of one of the leader's appends or heartbeats is
  younger than the flusher patience, a fixed 10 s. The leader appends an
  assignment when the group has none, or when the current flusher is no
  longer a learner or has been unhealthy for the patience, naming the
  first healthy replica in that order; with none healthy it appends
  nothing and keeps the current one. A replica that recovers does not
  take the role back, so a flapping replica moves the flusher at most
  once per patience. A new leader keeps the flusher its log names until
  it has led for the patience, so a failover does not move it. A
  flusher that leaves the tail placement while still a learner is the
  one planned move: §6.1's handoff reassigns it at once, with no
  patience or failover wait, to the first healthy remaining replica in
  order that has also mined through the commit point, and only that
  assignment's commit lets the learner go. It feeds committed frames to the miner and the record sink
  exactly as the local receiver does after the ack today, with the
  sink's size and age triggers unchanged (RFC 0014 §3.2, RFC 0063).
  Every other tail replica mines the same log for the tail (§6.7) and
  so can take over without replay. Two flushers during a reassignment
  write identical rows, which compaction deduplicates by position
  (§6.6).
- **Checkpoint.** RFC 0052's barrier runs on the flusher: cut at the
  last mined `seq`, publish everything buffered, then write a
  **cut-consistent bundle** for that `seq` to
  `<bucket>/wal/replicated/logs/<log>/checkpoints/<seq>/`, where `<log>`
  is the group's §6.6 log id (its bytes, and `checkpoint.json`'s, are
  below): the miner
  snapshot of every tenant in the group, the group's id-allocator cursor
  and its unused reserved blocks, and the effective miner configuration
  and algorithm version of every tenant (item 4 below), all taken at
  the same cut. A restore applies that configuration before it rebuilds
  any miner, never the process's own settings, and a bundle naming an
  algorithm version the binary does not support is refused, failing
  the restore closed rather than mining differently (B11). Only after
  every object of the bundle is durable does it write `checkpoint.json`
  (the `seq`, the term, the bundle's key) and propose a checkpoint entry
  to the leader. Publication is **monotonic and fenced**: bundles are
  immutable, keyed by their `seq`; `checkpoint.json` changes only by
  `put_if_match` and a writer whose `seq` is not above the stored one
  abandons its write; and the leader appends a checkpoint entry only
  from the flusher named by the latest flusher-assignment entry, with a
  `seq` above the last applied checkpoint, so a delayed former flusher
  can neither move the checkpoint back nor make a replica restore from a
  cut whose history is purged. Every replica snapshots and purges its group WAL below
  the checkpoint (RFC 0008 §6.7) when it applies that entry, so no
  history is purged that a miner restore still needs. Unlike RFC 0052
  §3.1 on the local backend, a failed snapshot write here **fails the
  checkpoint**: the barrier retries, and nothing is purged until a
  bundle is complete.
- **Highest published position.** The flusher also records, per group,
  the highest ingest position in any file it has published, in the
  footer statistics of every file, and as `published_high`, whose
  object and write rule are deferred to the forced-takeover amendment
  (§9, Q7); until it lands `checkpoint.json` carries no such field.
  `ourios wal force-leader` (B8) starts its new log above the larger of
  that and the survivor's log end, so a position
  that a lost replica flushed but never checkpointed is never issued
  again (§4.5). For the same reason it abandons every unused id block
  the survivor holds, from its log and from the checkpoint snapshot:
  the lost suffix may already have minted ids from them, and published
  rows with them. Before mining any new entry the survivor reserves
  fresh blocks from the store, whose compare-and-swap high-water is
  above every block ever reserved, so no template id is minted twice
  for different templates. Ids are not the only identity at risk: a
  widen raises `template_version` without a new id, so the lost suffix
  may have published `(7, 2)` while the survivor's snapshot still holds
  `(7, 1)`. Every published version has its audit event in the store
  first (audit-before-record, `ourios-ingester/src/publish.rs`), so
  before mining the survivor reads, per restored template, the highest
  version the tenant's audit trail holds, and its next widen of that
  template takes the version above it, so no `(template_id,
  template_version)` key ever names two token sequences.
- **Control entries.** The flusher assignment, the checkpoint, id-block
  and miner-configuration entries below, and §6.7's published and
  erasure entries are log entries the leader appends beside the tenant
  batches, so every replica applies them at the same position. In the
  group WAL each is a `ReplicatedEntry` record with its own tag, `0x04`,
  followed by `term: u64_le | index: u64_le` as every entry is, a
  one-byte `kind`, and that kind's body.

  | `kind` | Entry | Body |
  |---|---|---|
  | `0x01` | Flusher assignment | the flusher's replica UUID, 16 bytes big-endian |
  | `0x02` | Checkpoint | 33 bytes, below |
  | `0x03` | Id-block reservation | 16 bytes, below |
  | `0x04` | Miner configuration, algorithm 1 | 28 bytes, below |
  | `0x05` | Published (§6.7) | reserved |
  | `0x06` | Erasure (§6.7) | reserved |

  The bodies of kinds `0x02` to `0x04`, every integer little-endian and
  every UUID 16 bytes big-endian, as in `0x01`:

  ```text
  0x02 checkpoint    | proposer: uuid | cut term: u64 | cut index: u64 | highest kind: u8 |
  0x03 id block      | after: u64 | through: u64 |
  0x04 miner config  | similarity_threshold: f32 | similarity_floor: f32 |
                     | param_byte_limit: u32 | prefix_depth: u8 | max_node_children: u16 |
                     | max_templates: u32 | max_line_tokens: u16 | upstream_templates: u8 |
                     | upstream_template_byte_limit: u32 | upstream_association_limit: u16 |
  ```

  - **Checkpoint.** `proposer` is the replica that proposed it, the cut
    is the log id of the last entry its bundle has mined (the bundle's
    `seq` is the cut index), and `highest kind` is the log's highest
    kind at the cut. Every replica applies it only if `proposer` is the
    flusher in force at the entry's own position and the cut is above
    the last applied checkpoint's, below the entry, the log id of an
    entry in its log, with the log's highest kind at the cut equal to
    the body's; otherwise every replica ignores it alike, purging
    nothing, as the leader's own check (above) already should. Applying
    it makes it the group's checkpoint and builds a snapshot **at the
    cut**, not at the last applied entry: its last log id is the cut,
    and it holds the group's membership, flusher and highest kind as
    they stood at the cut, and the miner configuration and latest id
    block in force there,
    so a replica that installs it and then
    receives the entries after the cut mines every one of them from the
    bundle. To know them, the state machine keeps the positions at
    which the membership and the flusher changed since the last applied
    checkpoint, and the same for the miner configuration and the id
    blocks; that list
    grows only with those changes. `openraft`
    accepts a built snapshot below its applied index provided it is
    above the current snapshot, then purges below it.
  - **Id block.** The block `(after, through]` of RFC 0059 §3.3, with
    `after < through <= 2^63 - 1`; any other body refuses the log as a
    wrong length does. The leader appends it only after reserving it in
    the store, and the group's miner allocates from the blocks in log
    order. A block whose `after` is below the previous block's `through`
    is ignored by every replica: the store's compare-and-swap never
    returns one, and allocating it could issue an id twice. The
    previous block is the latest one applied, never an ignored one: the
    latest applied after the snapshot or, when none has been, the
    snapshot's, so a replica restored from a checkpoint and one that
    applied the whole log ignore the same blocks.
  - **Miner configuration.** The `MinerConfig` fields (RFC 0004 §3.2,
    RFC 0023 §3.1, RFC 0050 §3.2) in declaration order, each `f32` as
    its IEEE 754 bits and `upstream_templates` as `0` ignore, `1`
    observe, `2` adopt. A body that `MinerConfig` validation refuses
    refuses the log, as a wrong length does. It takes effect for every
    tenant of the group from its own position, and the entries before
    a group's first configuration entry are mined under that first
    entry, so a replica mines nothing until one has committed.
    `prefix_depth` and `max_node_children` shape the tree itself, and a
    snapshot's routes and mask tags are only valid at the depth it was
    mined at (`MinerCluster::restore_tenant` refuses a wildcard-routed
    leaf below a shallower walk), so the group's first configuration
    entry fixes them for the life of its log: a later entry that
    changes either refuses the log, as a wrong length does. Every other
    field takes effect from the entry's position on the tree as it
    stands, which a replica that mined through the change and one that
    restores a checkpoint cut after it hold alike. A checkpoint can
    purge every configuration entry, so the configuration in force is
    the latest in the log after the snapshot or, when it holds none,
    the snapshot's; a replica that opens from or installs the snapshot
    takes it from the record before applying any entry. The leader
    appends one when neither holds one, or when its own effective
    configuration differs from the one in force in any other
    field, carrying the in-force `prefix_depth` and `max_node_children`
    whatever its own; during a rollout that changes the configuration,
    the leader's wins.
    **Algorithm 1** is the miner this amendment ships. A miner change
    that mines any input differently under the same configuration is a
    new algorithm and takes a new kind, a miner configuration entry of
    algorithm 2 with that algorithm's body, so the kind gate above
    holds it back until every member reads it and every replica changes
    algorithm at the same position. That kind is the algorithm version
    the checkpoint bundle records. This entry carries the cluster
    default of RFC 0004 §3.4 only. The miner honours a per-tenant
    override, but no deployment surface sets one yet; when one does,
    the override travels in the log too, as a later kind naming the
    tenant, since a replica that mined a tenant under a local override
    the others lack would mint different templates.
    These rules are the two update primitives of DynSRV (Kristensen et
    al., §10) carried onto a replicated log: the fixed tree shape is
    its `defer`, set once by the first value, every other field its
    `dynamic`, the most recent value in force, and the kind gate is
    the guided adaptation it takes from Zhang and Cheng, where the new
    algorithm applies only once every member reads it. DynSRV updates
    one monitor at the step the update arrives. Here the update is an
    entry, and its log position is that step on every replica.
  A reserved kind's body is fixed by the amendment that implements it,
  and kinds are implemented in order: a release reads a kind only if it
  reads every kind below it. A release's highest readable kind,
  `0x00` when it reads none, is therefore exactly the set of kinds it
  reads.
  A replica that reads a kind it does not know, or a body of the wrong
  length, refuses to open the group's log rather than skip the entry:
  mining and flushing depend on applying every control entry in order.
  So the group's log has a highest kind, the highest it has ever held,
  and a leader appends a kind above it only once every member of the
  group, voters and learners, has answered its `Identify` call (§6.8)
  with a highest readable kind at or above the new kind. A kind at or
  below the log's highest kind needs no answers: every member already
  reads it, since a member joins only reading the log's highest kind
  (§6.8) and so, kinds being in order, every kind below. A lost member
  therefore never blocks the flusher reassignment that replaces it. A
  rolling upgrade holds new kinds back until it has reached every
  member, with no setting to change. The log's highest kind is the
  highest of the latest checkpoint's recorded value and every kind
  committed after that checkpoint, and the checkpoint body records it,
  so purging the log below a checkpoint loses none of it. A replica
  whose release reads below that recorded kind, one rolled back past a
  kind the group has used, refuses to open the group's log at the
  checkpoint rather than catch up into entries it cannot read; it
  serves and votes in no group whose log it has not opened, and rejoins
  only on a release that reads the group's highest kind. A leader
  gates on its own log, committed or not: the kinds of its uncommitted
  entries count too, so a learner it admits reads an entry of a new kind
  still in flight, and an entry a later leader truncates only ever made
  the gate stricter. The two changes never overlap: a leader appends a
  new kind only while no membership change is in flight, asking every
  member of its latest membership, and changes the membership only
  once its newest entry of a new kind has committed.
- **Template-id blocks.** The leader keeps reserved id blocks ahead in
  the log (as RFC 0059's refiller keeps three ready), topping them up
  when the flusher reports its remaining ids. A miner that exhausts them
  before the next block entry does so at the same log position on every
  replica, so the outcome (no id, body kept, as `id_reservation_failed`
  today) is still identical everywhere.
- **The high-water object's bootstrap.** A store root serves one WAL
  backend: running local receivers and replicated groups on the same
  root, or moving a root from one backend to the other, is unsupported
  until #1237 specifies it (§9, Q8). On a local root RFC 0059 §3.5 is
  unchanged, and no local start reads, creates or depends on
  `log_ids.json`. On a replicated root, whose store supports
  conditional writes (§6.2), RFC 0059's `miner/template_ids.v1.json` is
  created once, by one replicated start, and its absence afterwards
  always means it was deleted. `<bucket>/wal/log_ids.json` (§6.6) is
  the lock that decides which start that is, and it comes before the
  object. It is UTF-8 JSON:

  ```json
  {"format": 1, "bootstrapper": "ourios-receiver-0", "seeded": false, "next": 0}
  ```

  `format` is `1`; `bootstrapper` is the replicated `node_id` of the
  start that created the file; `seeded` says whether the high-water
  object exists under it; `next` is the next log id to allocate, a JSON
  integer read as a `u32`. Log ids start at `0`, so `next` is also the
  number allocated, and `0` means none has been. A reader takes it only
  with exactly these fields and `format` `1`; any other object, or one
  that does not parse, fails the start closed, and no node replaces it.
  - **The lock comes first.** A replicated start that finds
    `log_ids.json` absent creates it with `put_if_absent`, naming
    itself, `seeded` `false` and `next` `0`. Only the start whose
    create succeeds bootstraps: if no `template_ids.v<k>.json` exists,
    it runs RFC 0059 §3.5 with that section's table, `restored_max = 0`
    (no group holds an id before a log id exists) and its own node's
    `receiver.template_ids_allow_bootstrap`, and creates the object
    with `put_if_absent`, going on with the object it finds if one is
    present. It then sets `seeded` to `true` by `put_if_match`, and only
    then are log ids allocated, by `put_if_match` on `next`.
  - **A present lock is never bootstrapped past.** A replicated start
    that finds `log_ids.json` present never creates the object. With
    `seeded` `false` under another `node_id` it waits, reporting why,
    until `seeded` is `true`; with `seeded` `true` and the object
    absent, the object was deleted (§9 Q6), and it fails closed with
    `HighWaterDeleted`. A replicated start that paused anywhere before
    the object exists either holds the lock, so it bootstraps before any
    log id or block exists, or does not, so it never bootstraps: no
    start re-creates a deleted object from a floor below ids a group
    holds, whether or not that group has applied a block.
  - **A crash between the two steps.** A start that finds `seeded`
    `false` under its own `node_id` is a bootstrapper that crashed
    between the object and `seeded`. It repeats them and sets `seeded`.
    That is safe because no log id, and so no group and no reserved
    block, exists before `seeded`, the object's create is
    `put_if_absent`, and a `node_id` runs one start at a time (the
    StatefulSet's pod identity), so no paused start outlives its own
    restart.

  Thereafter only the leader writes the object, reserving blocks (RFC
  0059 §3.2), and a group's latest applied id block (§6.1's record,
  both `0` when none has been) stands in for `TEMPLATE_IDS_SEATED`'s
  `max_reserved_seen`. A leader appends no block while the object is
  absent, or while its `reserved_through` is below that block's
  `through`, and reports `HighWaterDeleted` or the rollback as RFC 0059
  §3.1 does; its miner then runs out of ids as above. RFC 0059 §3.1's
  re-bootstrap removes roots' markers and reaches neither
  `log_ids.json` nor a block held in a log (§9, Q6).
- **Snapshot and bundle formats.** The checkpoint bundle (§6.5) and the
  miner snapshots in it carry a format version; the snapshot's
  `wal_high_water` gains a variant `{ log, seq }` (both below). Once
  the WAL below a checkpoint is purged, a full replay can no longer
  rebuild the miner and allocator state, so the local backend's rule of
  discarding an unreadable snapshot and replaying does not carry over: a
  release that changes a bundle format ships a migration from the
  previous one, and a replica that finds neither a usable bundle nor the
  group's complete history from `seq` 0 **fails closed** for that group
  (it serves no tail and does not flush, and reports why) rather than
  mine rows that could differ from its peers'.
- **The miner snapshot's mark.** A bundle's miner snapshots keep
  `SNAPSHOT_VERSION` 3 and `ourios-miner`'s snapshot codec (byte 0 the
  version, then `serde_json` over `SnapshotState`). Only
  `wal_high_water` takes a second JSON shape, told apart by its keys:
  the local `{"segment": "<uuid>", "byte": <u64>}` and the replicated
  `{"log": <u32>, "seq": <u64>}`, `log` the §6.6 log id and `seq`
  the cut index, meaning the tenant's state folds every entry of the
  group's log through `seq`. An object with neither key set, or keys
  of both, is corrupt. The version is not raised: version 4 would send
  every local version-3 snapshot down the unknown-version path, a full
  replay that re-mints reclaimed ids (hazard #5), against RFC0065.16.
  The local writer still emits the same bytes, and the local loader
  treats a replicated mark as corrupt and applies the discard and
  replay it already uses; only a tail replica reads that shape, and only in a
  bundle. A release that raises `SNAPSHOT_VERSION` reads the previous
  version inside a bundle, the migration the bullet above requires,
  while the local backend keeps discarding it.
- **The bundle's bytes.** In the prefix
  `<bucket>/wal/replicated/logs/<log>/checkpoints/<seq>/`, `<log>` is
  the log id in decimal and `<seq>` the cut index as 20 zero-padded
  decimal digits, so key order is cut order. It holds
  `tenants/<tenant>.snap` for each tenant whose state the group's miner
  holds at the cut, `<tenant>` percent-encoded by RFC 0005 §3.4 as
  `snapshot_store.rs` names its files and the bytes exactly what
  `snapshot()` writes for it, and `bundle.bin`, every integer
  little-endian:

  ```text
  format: u8 = 1 | log: u32 | seq: u64 | term: u64 |
  next: u64 | current after: u64 | current through: u64 |
  blocks: u32 | blocks × (after: u64 | through: u64) |
  tenants: u32 | tenants × (len: u16 | tenant: len bytes | config kind: u8 | config body |
                            snapshot len: u64 | snapshot crc32c: u32) |
  crc32c: u32
  ```

  `log`, `seq` and `term` are the log id and the cut's index and term.
  `next` is the next id the allocator issues and `current` the block
  `(after, through]` its range comes from, `IdRange`'s `end` being
  `through + 1`; before the allocator has started a block, `current` is
  `(0, 0]` and `next` is `1`. `blocks` are the applied blocks it has
  not started, in log order. The allocator keeps `current` for this,
  since its range alone cannot show which block it came from. Each
  tenant is its id's UTF-8 bytes as
  the `TenantOtlpBatch` prefix carries them (RFC 0046 §3.3, `len` 1 to
  128), sorted bytewise, none repeated; `config kind` and `config body`
  are its effective miner configuration, the control-entry kind that is
  its algorithm version and that kind's body (`0x04` and its 28 bytes);
  `snapshot len` and `snapshot crc32c` are its `.snap` object's length
  and CRC. Every CRC is CRC32-C (Castagnoli) as in RFC 0008's frames,
  and the last covers every byte before it.
  - **Reading.** A replica restores from a bundle only if `format` is
    `1` and the CRC holds; `log`, `seq` and `term` are the log id and
    the cut it restores (§6.1's record); `current` is `(0, 0]`
    with `next` `1`, or has `after < through <= 2^63 - 1` and
    `after < next <= through + 1`, so `next` is at least `1` and `0`,
    the `NO_TEMPLATE` sentinel, is never issued, and no id at or below
    the block's `after` is; every pending block has
    `after < through <= 2^63 - 1`, the first's `after` at or above
    `current`'s `through` and each next one's at or above the previous
    `through`; the last pending block, else `current`, equals the
    record's latest id block in both `after` and `through`; the tenants
    are sorted and
    unique; each tenant's configuration kind is one it reads, its body
    passes that kind's checks (`MinerConfig` validation for `0x04`),
    and, while no per-tenant override is specified, kind and body
    equal the record's configuration in force byte for byte; and each
    `.snap` has its length and CRC, decodes under
    `load_snapshot`, carries the mark `{log, seq}` with this log id and holds no
    template id at or above `next`. Any other bundle, or one missing an
    object, fails the restore closed as above. It never lists the
    prefix: the tenants are the ones `bundle.bin` names.
  - **Writing.** Each object is created with `put_if_absent`. A put
    that finds its object present reads it and goes on only if the bytes
    are the same, as a retry of its own put's are, and otherwise
    abandons the checkpoint, so no bundle object is ever overwritten.
    An object is durable once its put returns on the S3-compatible
    stores this backend requires (§6.2); a store whose put is not, the
    local filesystem's, syncs it first with `sync_local_blocking`, as
    RFC 0059 §3.2 step 4 does. The flusher writes `bundle.bin` only
    once every `.snap` is durable, `checkpoint.json` only once
    `bundle.bin` is, and proposes the checkpoint entry only once
    `checkpoint.json` is, so an applied cut always names a complete
    bundle its proposer wrote. Two flushers' bundles for one cut need
    not be the same bytes.
- **`checkpoint.json`** is
  `<bucket>/wal/replicated/logs/<log>/checkpoint.json`, UTF-8 JSON:

  ```json
  {"format": 1, "log": 3, "seq": 200, "term": 4,
   "bundle": "wal/replicated/logs/3/checkpoints/00000000000000000200/"}
  ```

  `format` is `1`; `log` is the log id; `seq` and `term` are the cut's
  index and term; `bundle` is the bundle's prefix, relative to the
  bucket. Every number is a JSON integer read as a `u64`, `log` as a
  `u32`. A reader takes the object only with exactly these fields,
  `format` `1`, `log` the group's and `bundle` the prefix `log` and
  `seq` name. §6.5's `published_high` is not among them yet: where it is
  kept and when it is written belong to the forced-takeover amendment
  (§9, Q7). Any other object, or one
  that does not parse, is unreadable, and a writer never replaces it,
  since a whole-object rewrite would drop a field it does not know: its
  checkpoint fails as a failed bundle write does, and it reports why.
  The object is created with `put_if_absent` and changes only by
  `put_if_match` on the ETag the writer read. A writer that reads a
  stored `seq` at or above its own abandons its write (above);
  otherwise it writes its `seq`, `term` and `bundle`; a lost race
  re-reads and decides again. Recovery does not read the object
  (below).
  - **Keyed by log id, not group number.** A log incarnation that
    replaces another over a bucket that keeps the old one's objects
    starts its Raft indexes over, and under the group number its writes
    would lose for good to the old object's higher `seq`.
- **Recovery** of a tail replica on start: the mark is one cut. The
  replica opens the group's log from §6.1's snapshot record, whose last
  log id is the cut `c` of the last checkpoint it applied, and every
  tenant's snapshot in that bundle is taken at `c`. With a record, it
  restores every tenant's miner, the allocator and the configuration
  from the bundle at `c`'s index, never a later one `checkpoint.json`
  names, whose entry may not have applied, and mines the group WAL
  from the entry after `c`; without one, its log holds the group's
  history from its first entry, and it mines all of it. A replica that
  installs a snapshot does the same at its cut. It suppresses per
  consumer as RFC 0008 §6.6 does, and serves the tail once it has mined
  through the leader's commit point. A replica that cannot read the
  bundle, the store unreachable or the bundle refused, fails closed for
  the group as above and retries until it restores; it keeps taking
  the log as a learner, so the group's acks do not depend on it (B10).
  The outage of RFC0065.21 writes no checkpoint, so the logs keep every
  entry above the last cut and a tail replica that stays up serves the
  whole backlog. One that restarts during the outage cannot restore and
  serves none of it until the store returns, and a group none of whose
  tail replicas serves reports its tail unavailable (RFC0065.14).

### 6.6 Position as the dedup key (B13, H8)

Every row mined on this backend carries its **ingest position**
`(group: u32, seq: u64, index: u32)`, where `index` is the record's
place in its frame. Mining reads only committed entries, and a committed
`seq` is never reused (§6.4), so the position names one acknowledged
record.

- **Storage.** **Amends RFC 0005 §3.2:** three new `OPTIONAL` columns,
  `ingest_group` (`UInt32`), `ingest_seq` (`UInt64`) and `ingest_index`
  (`UInt32`), written with dictionary encoding off and
  `DELTA_BINARY_PACKED`. Old files and local-backend files have no such
  columns, and readers treat them as absent (RFC 0005 §3.9), so
  `AGENTS.md` §3.5 needs no migration.
- **A log namespace in the group number.** `ingest_group` is not the raw
  group index but a **log id** allocated once per log incarnation: the
  first start of a cluster allocates one id per group from a counter in
  `<bucket>/wal/log_ids.json` by compare-and-swap, and records the
  mapping in `cluster.json`. RFC 0066 allocates one per (topic id,
  partition), using the broker's topic UUID, so a recreated topic gets
  new ids. Positions from different logs, backends or topic incarnations
  therefore never compare equal, and switching a tenant's backend or
  recreating a topic cannot make dedup discard unrelated rows.
- **Cost, measured** (2026-11-01 spike, otel-demo-v4, 735 377 rows at
  28 B a row on disk, the real writer properties): 0.03 B a row (0.1 %)
  in freshly flushed files, which hold ascending runs, and 0.9 B a row
  (3.2 %) after compaction's RFC 0036 re-sort. A `FixedSizeBinary(16)`
  column cost 0.5 B and 2.1 B (7.6 %); any dictionary-encoded integer
  form was worse still, since every position is unique.
- **Kept for the file's life.** Late writes land in already-compacted
  partitions as new files (RFC 0009 §3.3), and a new leader re-flushes
  rows above the checkpoint, so a sealed partition can still receive a
  duplicate, and only the columns let the next compaction remove it.
  The erasure rewrite (RFC 0009) carries them through.
- **Never read by queries.** Selective scans prune without them, and
  materialization projects them out; the spike found the record
  materializer reading the whole union schema, which the implementation
  must narrow.
- **Compaction** (RFC 0009) keeps the first row of each
  `(tenant, ingest position)` and drops the rest. **Amends RFC 0009
  §3.5:** row count and per-`template_id` counts stay invariant except
  for those repeats, and RFC0009.2 is restated to count distinct
  positions where the columns are present.
- **Queries** dedup the tail against Parquet by it (§6.7).
- **What it does not remove.** A client retry after a lost ack is a
  new entry with a new `seq`, the same duplicate the local backend makes
  today. Ourios adds no key of its own to remove it; the delivery is
  documented as at least once, as OTLP's is (§9, Q4).

### 6.7 The tail service

A querier holds neither the group's log nor its miner state, and B23
rules out serving the tail from the replica that acknowledges (§7). So
the tail is served by
**tail replicas**: replicas that receive the group's committed log but
never count toward an ack (learners, under §4.3's shape).

- **What a tail replica holds (B19).** The mined rows of every
  acknowledged record not yet in the object store, with the template
  definitions those rows use, in the RFC 0033 map's shape. Recent rows
  are in memory; the rest are in **local Parquet files**, written by the
  same writer and properties as the object store's (RFC 0005) as soon
  as a size or age trigger fires, and kept until the checkpoint passes
  them. On the flusher those are the files it uploads once the store is
  reachable, so a long object-store outage leaves the whole backlog
  queryable with the same pruning as remote Parquet. They are a cache
  derived from the WAL and rebuilt from it after a disk loss, which
  `AGENTS.md` §3.6 allows.
- **The tail volume is bounded.** A tail node keeps, for every group
  it hosts, the group's learner WAL above the checkpoint (B25 bounds
  it, per group, as on the voters) and the local Parquet cache of the
  same rows, which is no larger than that WAL. Rendezvous placement
  can give one tail node far more than the average share of groups,
  and a tail-pool change moves groups, so the chart does not size from
  an average: by default a tail volume holds twice what a node hosting
  **every** group needs, 24 hours of the stated ingest rate for the
  learner WAL and again for the cache, which no placement or
  reassignment can exceed. Ourios reports the groups each node hosts
  and its tail WAL and cache bytes, alerts at 50 % and 80 % of the
  volume, and an operator who sizes smaller from the reported
  placement does so knowingly. If the volume
  fills anyway, the replica stops caching new rows for the group that
  needed the space and reports that group's tail unavailable (B17)
  until the checkpoint frees space; acks never wait on a tail volume,
  and no acknowledged record is lost, since the WAL still holds it.
- **Where the mined rows come from: a deterministic miner.** Every tail
  replica mines the same committed log from the same snapshot and gets
  the rows the leader gets, so only raw frames are replicated. A spike
  (2026-11-01, four corpora up to 200 000 records) found the mining
  algorithm already deterministic: twin miners on different threads,
  and a miner restored from a mid-stream snapshot, produced identical
  rows; candidate ties break on the lowest id and snapshots are sorted.
  Only `template_id` differed, and only through RFC 0059's allocation:
  store-reserved blocks give two miners on one store disjoint ids, a
  restart reseats above the store's high-water, and one id space is
  shared by every tenant in a process. So, **coordinating with RFC 0059
  (`green`)**:
  1. **Id reservations are log entries.** The leader appends an
     "id block reserved" entry to the group log, after reserving the
     block in the store as today; every replica allocates from the
     blocks in log order. RFC 0059 §7 rejected WAL-journalled
     reservations only because they change the WAL format, which this
     RFC changes anyway.
  2. **One id space per group**, not per process; uniqueness across
     groups still comes from the store's compare-and-swap blocks (RFC
     0001 §3.7.2).
  3. **Restarts restore from the checkpoint snapshot plus the log's
     reservation entries**, not by reseating; the store's high-water
     stays a floor, so an id is still never reused.
  4. **Miner configuration and version are pinned per epoch**: a
     per-tenant override or a miner upgrade takes effect at a log entry,
     so replicas never mine one entry with different settings.
  5. **Audit timestamps** come from the entry, not the wall clock.

  None of this changes merges, body retention or reconstruction
  (`AGENTS.md` §3.1–§3.3). A tail replica whose rows diverge from the
  flusher's anyway is a bug B16 catches (RFC0065.20).
- **Read your acks (B14) without touching ingest (B23).** A `Tail` call
  to a tail replica first obtains a **quorum-confirmed** read point:
  the replica asks the leader for its read log id, which the leader
  returns only after confirming its leadership with a quorum (ReadIndex,
  not a lease), so a deposed leader or one whose commit point lags
  cannot answer. One confirmation serves every read queued before it
  was issued (§4.6: 4.5× fewer confirmations); `openraft`'s piggyback
  option, which waits for the next heartbeat instead, is the setting
  for read-heavy deployments.
  The replica then waits until it has applied and mined through that
  id, and answers. If it cannot obtain a confirmed read point it answers
  "unavailable" instead of a possibly stale tail.
- **Templates and aliases (H5).** The querier compiles body-equality
  predicates and renders rows against the stored template map (RFC
  0033) before it plans (`ourios-querier/src/lib.rs`). Tail rows can
  use templates that map does not hold, so the tail side does not
  depend on it: the `Tail` scan carries the query's predicates (B19),
  and the tail replica resolves body predicates against its own
  template state, which holds every template its rows use, restored
  from the snapshot or minted since, with the same ids on every
  replica (§6.7). With the rows it returns it sends the definitions
  (`template_id`, `template_version`, tokens) of only the templates
  those rows reference, so the exchange grows with the result, not the
  backlog, and a `count` sends none. For the Parquet side the querier
  acquires its map **after** step 3 below fixes the read point: every
  file it then reads, listed or named by the tail, was published
  before the map was acquired, and audit-before-record puts that
  file's templates in the store first, so RFC 0033's "re-derive, never
  serve stale" map holds them; a row flushed after the read point is
  still a tail row at that point. Alias state is not produced by mining
  and is read as today. During an object-store outage the querier uses
  its last refreshed map and alias state (RFC 0060) for nothing but
  aliases, since the Parquet it would apply to is reported
  unavailable.
- **The querier's read**, per group the query's tenant is in:
  1. read the group's checkpoint `c`;
  2. list the tenant's Parquet files as today (`file_set.rs`);
  3. call `Tail` on any tail replica of the group with `from = c`; it
     fixes a read point and answers with the keys of the tenant's files
     published above `c` as of it, and the plan's tail scan
     (`TailScan`) later returns that tenant's **unpublished** rows at
     or above `c` as of the same read point, or "too old" if `c` is below the oldest row it keeps,
     in which case the querier restarts at step 1 (bounded retries).
     Those keys are hints: compaction or erasure may have retired a
     file since, so the querier re-resolves each key's partition
     through its current manifest (RFC 0009 §3.4) and reads only the
     live files there, replacement outputs included, never a retired
     key;
  4. deduplicate by ingest position across the whole input, Parquet
     and tail together, before any aggregation or projection drops the
     key. Candidates are chosen by **position**, never by file name or
     time: from the footer statistics of the files the query already
     plans, the querier finds the files whose `ingest_seq` range for a
     group overlaps another file's or the tail's range, and only rows in
     those files and the overlapping tail rows go through a
     dedup-by-position step; every other file is scanned as today. A
     freshly flushed file holds one ascending run of positions per
     group, so in steady state only the newest files overlap the tail,
     and the range test costs footer reads (88 µs in the spike).

  A flush between the listing and the call is caught by the published
  file keys the tail returns, so its rows are read from the store, not
  the tail. Two live
  files holding one position (a flusher reassignment, a replay after a
  failover) overlap in range and are deduplicated the same way, before
  compaction removes the copy. So every acknowledged record is counted
  once (B16), whatever a clock said when a file was named.
- **During an object-store outage** the querier cannot read
  `checkpoint.json` or list remote files. It then takes the checkpoint
  from the tail replica, which holds it as the last applied checkpoint
  entry, and the tail replica serves every unpublished row above it from
  memory and its local Parquet files. Remote Parquet is unreachable,
  both below the checkpoint and the files published above it: the
  response returns the tail's rows and names those as unavailable (`parquet_unavailable`, alongside
  B17's `tail_unavailable`), so nothing is silently missing. Every
  record acknowledged during the outage is above the checkpoint, which
  cannot advance without the store, so the whole outage stays
  searchable (RFC0065.21).
- **Published rows are served from the store only (erasure).** After
  each file it publishes, the flusher proposes a **published entry**
  naming the file's key and the positions it holds; the leader accepts
  it only from the current flusher, as for checkpoint entries. A tail
  replica that applies it stops serving those rows and keeps only the
  key, which it returns to `Tail` callers (step 3) until the checkpoint
  passes it. So once a row is in the object store, the store is its only
  source (`AGENTS.md` §3.6), and an erasure (RFC 0047 §3.6, RFC 0009)
  that rewrites a published file is never undone by a cached copy.
- **Erasure goes through the log.** A flusher can pause after writing a
  file and before its published entry commits, and an erasure can
  rewrite that file meanwhile, so the published entry alone is not a
  fence. Before its confirming pass, the compactor proposes an
  **erasure entry** (the tenant and the erasure's predicate, RFC 0047
  §3.6) to every group the tenant is in, through any receiver as an
  export is (B9), and waits for each to commit; an erasure whose entry
  cannot commit stays incomplete and is retried, it is never reported
  done. A replica that has applied the entry serves none of the
  matching rows below its position, and a flusher that has applied it
  writes none of them to a file it publishes afterwards. So a cached
  tail copy never outlives an erasure: the case of a paused publication
  whose file the erasure has already rewritten is closed. Committing
  the entry does not prove that every flusher has applied it, though:
  a lagging current flusher, a paused former one or a re-flush after
  failover can still publish matching pre-request rows after the
  confirming pass. That is exactly RFC 0009 §7 D3's accepted residual
  ("a pre-request row whose flush publishes after that pass is not
  erased"), unchanged in scope on this backend and pinned by
  RFC0009.22; the tail never serves those rows. Closing it needs a
  barrier proving every possible publisher has applied the entry,
  which D3 leaves to a later RFC; this RFC does not amend RFC 0009
  §3.4's live-set rule.
  Mining still
  consumes those entries, so every replica's miner stays identical
  (B11).
- **Scaling (B24).** Tail replicas come from the tail pool (§6.1),
  `tail_replicas` per group (default 1); the querier spreads `Tail`
  calls across them. Adding tail nodes or raising `tail_replicas` is a
  learner-only membership change: neither the voters, the write quorum
  nor the ack path change.
- **Unavailable (B17).** If no tail replica of a group answers past a
  bounded retry, the response carries the Parquet results and a
  `tail_unavailable` entry naming the group. The query API and the MCP
  tools gain that field; a client that ignores it sees today's
  behaviour. A request with `require_complete: true` (default `false`)
  instead fails with the retryable unavailable error (HTTP `503`, an
  MCP tool error) naming what was unreachable, and returns no rows; it
  applies to `parquet_unavailable` alike. The wire form:
  - Request: `require_complete`, an optional JSON boolean on the RFC
    0016 query body and the MCP `query_logs` arguments.
  - Response: two optional top-level arrays on the query response (RFC
    0016's `LogQueryResponse`) and in the MCP tool's structured result,
    omitted when empty: `tail_unavailable`, objects `{ "group": <u32> }`
    with the group index of §6.1, one per group and sorted by it; and
    `parquet_unavailable`, objects `{ "key": "<object key>" }`, one per
    file, sorted by key. A store outage that hides a whole listing
    reports `{ "key": "<partition prefix>/" }` for the prefix it could
    not list.
  - The `503` body is RFC 0016's `{ "error": { "kind", "message" } }`
    with `kind` `"incomplete"`, plus the same two arrays beside
    `error`; the MCP tool error carries the same object as its
    structured content. Dashboards keep the default;
  alerting rules set the flag, so a missing tail never reads as
  "nothing happened".
- **Same plan (B18).** The tail is a DataFusion table beside the
  Parquet listing, so tenant and conversation predicates, row masking
  (RFC 0047) and the DSL apply unchanged.
- **Cost (B19).** A `Tail` call prunes the local files by their footer
  statistics exactly as the querier prunes remote ones, so it reads
  what the query's range and predicates select, not the backlog. Position
  dedup reads footer statistics of the files already planned, and the
  three position columns only from files whose ranges overlap.

### 6.8 The peer protocol

- A gRPC service, `ourios.wal.v1.Replication`, on `listen_addr`, with
  `openraft`'s peer calls (§4.3): `StreamAppend` and `Append` (leader
  to follower, entries and heartbeats), `Vote` and `PreVote` (candidate
  to voter), and
  `InstallSnapshot` (leader to a follower behind the checkpoint); then
  `Forward` (any node to a leader), `Identify` (a leader, or a group's first-ranked voter
  forming its first membership, to a configured peer: the `node_id`
  and replica UUID it runs under, §6.1),
  and `Tail` and `TailScan` (querier to tail replica: the read point and
  published keys, then the rows under that read point, §6.7). Catch-up needs no call of its own: the
  leader sends a lagging follower its missing entries by
  `StreamAppend`, or the checkpoint by `InstallSnapshot` when they are
  already purged.
- `Identify` names no group and changes nothing: the node answers with
  its `node_id`, the UUID in its `replica.id`, and the highest control
  entry kind it reads (§6.5). Kinds are numbered in the order they are
  introduced and a release reads every kind up to its highest, so one
  number states what a node can apply. A kind above the group's highest
  kind is appended only once every member has answered with a kind at
  or above it, a member that has not answered counting as below. A kind
  at or below it needs no answers: every member then reads it, since a
  learner is added only if its kind covers every kind in the leader's
  log, committed or not, and a new kind and a membership change are
  never in flight together (§6.5). So a lost
  member holds back a new kind but never the reassignment of its own
  flusher, and no admitted replica is asked to apply an entry it cannot
  read. A member rolled back afterwards, under its UUID, is not
  admitted again, so the check is also its own: a replica refuses an
  append that carries a control kind above the highest it reads before
  it writes or acknowledges it, as well as such an entry already in its
  log or checkpoint (§6.5), and stops serving and voting in that group
  as if it were down, until a release that reads the kind restarts it.
  A leader calls each
  peer of a group's tail placement, and each voter of the group, at
  the address its own configuration gives, and acts on the answer
  through a committed membership change only: it adds a node it does
  not hold as a learner under the UUID it reported, and starts §6.1's
  replacement for a voter's `node_id` that answers under a new UUID. An
  unreachable peer is identified on a later pass, and an answer whose
  `node_id` is not the one the leader called is ignored.
- `Forward` carries one export to the node the sender believes leads
  its group: the group, the `TenantOtlpBatch` payload and the
  credential the client presented, if any. It names no target replica.
  The leader authenticates that credential and checks the tenant
  binding again (RFC 0026) before it appends, exactly as if the client
  had sent the export to it, and answers with the committed position,
  the same rejection the client would have got, or `UNAVAILABLE` with
  its leader hint when it does not lead the group. A node never
  forwards a forwarded export again.
- The sender tries another node only after an outcome that proves
  nothing was appended: the receiving node does not lead the group or
  does not host it, or the connection was refused or not made within
  the peer connect timeout (1 s, covering the TLS handshake), so the
  call was never sent. It tries at most `replication_factor` nodes,
  the hint first and then the group's voters in placement order. A
  rejection the leader gives before appending, such as a credential
  or tenant binding it refuses, also proves nothing was appended; the
  sender passes it to the client unchanged and tries no other node.
- Once sent, a call is bounded by the commit timeout plus the connect
  timeout. Any other outcome (that deadline, the commit timeout, a
  lost or failed answer, a leader that lost leadership after
  appending) is uncertain: the entry may still commit, so the sender
  answers the client with the retryable rejection and does not
  resubmit, which would append the export again under a second index
  that §6.6 cannot remove (§9 Q4).
- RFC 0061 bounds the client's request. The forwarded frame is
  bounded by RFC 0008's frame size cap, as the WAL frame it becomes
  is, and a `Forward` message by the peer message limit, one
  maximum-size frame plus its envelope, so an export the leader would
  accept directly is never refused for the tenant prefix.
- Replication runs over `StreamAppend`, one bidirectional stream per
  `openraft` replication session, so the leader sends appends without
  waiting for the answers to earlier ones (§4.6). Every message on the
  stream is an append request with its own header, and every header
  must equal the first: a stream addresses one replica of one group. An
  append larger than one message travels as several, each a valid
  append the follower checks on its own. The follower answers each
  message in order: a match, once its log has flushed the message,
  with the log id it now matches through (absent for a heartbeat on an
  empty log); or, without a flush, the `prev_log_id` it does not hold
  or a higher vote. The stream ends after a conflict or a higher vote,
  and when the group stops or the server shuts down. `Append`, the unary form,
  carries one append and answers it.
- Each request but `Identify` names its group, and one connection per
  peer carries every group's calls; a server answers an unknown group
  with `NOT_FOUND`. `Identify` carries only the protocol version, so a
  node answers it before it hosts any group.
- **Mutual TLS is required** (RFC 0030's `TlsSection`). A tail replica
  accepts `Tail` and `TailScan` only from a client certificate whose identity (its
  URI or DNS subject alternative name) is listed in `tail_clients`, the
  queriers' identities the chart renders from the querier certificate
  secret; any other caller, another replica included, gets
  `PERMISSION_DENIED` and no rows. A `Tail` call carries the query's
  tenant, already authorized by the querier (RFC 0047), and the tail
  replica returns only that tenant's rows. Trusting the querier for the
  tenant adds no authority: a querier already reads every tenant's
  Parquet with its store credentials, so a `Tail` call gives it nothing
  the store does not.
- The protocol carries a version, and a node rejects a peer with an
  unknown version (B22).

### 6.9 Telemetry

- Ourios-owned names go through the shared `ourios-semconv` registry,
  checked against the OTel conventions first: per group, follower lag
  in entries, leadership changes, and the term (the fallback's epoch);
  with the fallback, also the in-sync set size and compare-and-swap
  conflicts on the group record; forwarded exports;
  tail calls, their latency and `tail_unavailable` responses; and H8's
  two detection signals, the dedup hit count and
  `bytes_stored / bytes_received` per tenant.
- Peer RPCs are traced as gRPC client and server spans under the
  existing RPC conventions (RFC 0038 §3.5); a query's span covers its
  `Tail` calls.
- Rejections use `error.type` on the existing `ourios.ingest.batches`
  counter: `not_enough_replicas` and `group_unavailable`.

**Defaults are starting values, tuned from these signals.** §3 fixes the
behaviour; the numbers in it and in §6 are first settings, to be moved
by production telemetry rather than by further design. Each has the
signal that tunes it:

| Default | Starting value | Tuned from |
|---|---|---|
| Backlog limit (B25) | 90 % of the WAL volume, sized for 24 h | backlog bytes and age per group, time to the 50 % alert in real outages |
| Catch-up rate (B26) | ≥ 2× the sized ingest rate | flush throughput against ingest throughput per group, measured drain times |
| Groups | 16 (about 0.06 cores of heartbeats; §4.6) | per-group CPU, memory and heartbeat cost; ingest skew across groups |
| `tail_replicas` | 1 | tail call latency and saturation per tail replica |
| Tail memory before local Parquet | the record sink's size and age triggers | tail replica memory, local file count and query bytes read |
| Tail volume | 2 × 24 h of the stated ingest rate (a node hosting every group); alerts at 50 % and 80 % | groups hosted and tail WAL and cache bytes per node during real outages |
| Election and heartbeat timeouts | 50 ms heartbeat, 150–300 ms election: failover and tail gap about 1.8 × the election maximum, 0.55 s (§4.6) | failover time, spurious leadership changes |
| Vote log | one per node, group-committed (§4.7): a node failure at 128 groups failed over in 0.5–0.8 s | failover time when a node's groups elect at once, vote-log fsync latency and votes per fsync |
| Freshness target (B15) | under 1 s | ack-to-queryable latency, a new histogram on the tail path |

The backlog bytes and age, flush and ingest throughput per group, and
the ack-to-queryable latency are new names in the same registry as the
rest of this section.

### 6.10 Helm

- `receiver.wal.backend: replicated` keeps the receiver StatefulSet and
  its volume claims as the voters, and renders `peers` from
  `receiver.replicas` and the existing `-receiver-headless` Service
  (`ourios-receiver-<i>.<service>:4320`), with `node_id` from the pod
  name.
- The tail pool is a second StatefulSet, `-wal-tail`, with its own
  `walTail.replicas`, volume claims (by default twice 24 hours of the
  stated ingest rate, §6.7) and headless Service, rendered into `tail_peers`, and
  `tail_clients` rendered from the querier certificate's identity.
  Scaling it changes `tail_peers` only, never `peers`, so the voting
  layout check (§6.1) is unaffected (B24).
- The peer port is on the headless Service only. Peer and querier
  client certificates are mounted from secrets named in values, as RFC
  0030's listener certificates are.

### 6.11 Invariants and hazards

- **§3.1–§3.3 (miner):** unchanged. Frames reach the miner whole and in
  per-tenant order, on one node at a time.
- **§3.4 WAL-before-ack:** per RFC 0064 §3.1. Every replica's copy is an
  RFC 0008 WAL with its fsync rules; the ack waits for the fsync on
  every in-sync member; the floor is never breached; a replaced leader
  is fenced.
- **§3.5 schema:** one optional column; old files read unchanged.
- **§3.6 object storage as truth:** group records, checkpoints,
  snapshots and the cluster layout live in the store. Local disk holds
  only the WAL above the checkpoint and caches derived from it (the
  tail's memory and local Parquet files), all rebuilt from the WAL and
  the store after a disk loss.
- **§3.7 tenancy:** the tenant is in every frame and every tail row.
- **H3:** the batch window stays the knob; RFC0065.3 is this backend's
  crash test.
- **H4:** unchanged; the leader flushes as a receiver does today.
- **H6:** the DSL is unchanged; the tail is a table under the same plan.
- **H8:** live on this backend, met by §6.6.

## 7. Alternatives considered

**The hand-written in-sync design (§6) instead of Raft.** Its one
real gain is surviving one loss with two replicas, because the store's
compare-and-swap acts as the tiebreaker. It costs an ack that waits for
the slowest in-sync member (up to `replicate_timeout_ms` before a slow
one is dropped), elections that depend on the store, and a protocol
with no published testing. §4.3 prefers `openraft`.

**`raft` (raft-rs) or `omnipaxos` instead of `openraft`.** §4.1: no
release since 2023 and a flagged `protobuf` dependency, or an
in-development library with no production users.

**A Kubernetes Lease for leadership.** It ties correctness to one
orchestrator (B20), and a Lease is itself a compare-and-swap object
with a clock-based expiry, so it adds nothing the store does not give.

**A Kafka-protocol broker as the WAL.** RFC 0066. It removes the
receivers' disks and gives a shared log to read the tail from, but adds
an external system and acknowledges without fsync.

**Replicate the mined rows instead of mining on tail replicas.** The
leader would append its rows to the group log as a second entry kind,
and tail replicas would never mine. Measured on the corpus, mined rows
as Arrow IPC are 2.3–5.8× the raw frame bytes uncompressed and 2.5–3.6×
with zstd (resource attributes repeat on every row), so replication
traffic would grow 3.3–6.8×, plus an entry kind with its own retention.
Rejected for the deterministic miner (§6.7).

**Serve the tail from the leader.** The leader already holds the mined
rows, so it needs no second miner and no extra replication. But every
tail query would then run on the replica that acknowledges, mines and
flushes, so query load would compete with ingest and ingest capacity
would cap query capacity. Rejected by B23 and B24.

**Let the querier read raw frames and mine them itself.** A second
miner assigns different template ids, so `count by template_id` would
disagree between tail and Parquet.

**Replicate the record sink's buffer instead of the WAL.** The buffer
holds mined rows, so a replica could not re-mine after a miner change,
and bit-identical bodies (`AGENTS.md` §3.3) would have to be rebuilt
from rows.

## 8. Testing strategy

- RFC0065.1, .8's golden values: unit tests in `ourios-server/src/config`
  and next to the hashes.
- RFC0065.2, .4–.9, .12–.15: a deterministic in-process cluster in
  `ourios-ingester`'s test tree: three receivers and a querier on the
  in-memory store (which supports conditional updates and can be made
  unreachable), a simulated network that can drop, delay, reorder and
  partition, and a controllable clock; crash loss cuts each segment
  back to the offset of the last `Wal::sync` (§4.4). These run locally
  and in CI on every PR, with no containers. `turmoil` and a
  filesystem shim in `ourios-wal` are a later refinement.
- RFC0065.3 and RFC0065.11's latency arm: served-binary tests in
  `ourios-server/tests/it`, beside the existing WAL crash-recovery
  tests, run by CI's `test` job (`cargo nextest`); RFC0065.3 adds a
  multi-process variant that SIGKILLs the leader, as RFC 0064 §3.2
  requires.
- RFC0065.10: with `openraft`, election and replication rest on its
  upstream TLA+-checked design and test suite, re-run on every pin
  bump; the Ourios parts (checkpoint publication, flusher handoff,
  dedup, the tail read barrier) are covered by the seeded simulation
  suite plus their targeted scenarios (RFC0065.12, .23, .25, .26). We
  write no TLA+ of our own: those parts are small, each known hazard
  in them has a scenario, and a second specification language and
  toolchain for a single-maintainer project would cost more than it
  finds. With the §6 fallback, a model of the whole protocol, checked
  in CI to a bounded depth, since nothing upstream covers it.
- RFC0065.29: codec and open tests in `ourios-wal` and the group log
  beside the existing record codec tests, and the leader's kind gate on
  the in-process cluster.
- RFC0065.30: the in-process cluster with tail learners, the patience
  shortened for the test and each failure driven by a gate or a kill.
- RFC0065.26 and .31's bundle arms: codec tests beside the bundle and
  `checkpoint.json` readers, one per refusal, and the in-process
  cluster on the in-memory store, with a failpoint between each pair of
  bundle puts and the store made unreachable across a tail replica's
  restart.
- RFC0065.16: the existing suites, unchanged.
- RFC0065.31's lock arms (two replicated starts, a crashed
  bootstrapper, a deletion with groups holding blocks): the in-process
  cluster on the in-memory store, with failpoints between the lock,
  the object and `seeded`.
- RFC0065.17: `deploy/helm/render-tests.sh`.
- RFC0065.18, .19: `criterion`-style load runs on the in-process
  cluster (ack latency with and without tail load; tail throughput with
  one and two tail replicas), indicative on the CI runner first.
- RFC0065.27: the same in-process cluster with real per-group WALs and
  the vote log on disk, durable fsync on, killing one node; indicative
  on the CI runner first, with §4.7's runs as the baseline.
- RFC0065.28: the in-process cluster with the compactor's erasure
  pass on the in-memory store, the flusher paused by a failpoint
  between its object put and its published entry.
- `criterion`: ack latency on the replicated backend against the local
  one, and the tail merge, before `validated`.

## 9. Open questions

The behaviour questions were resolved by the maintainer on 2026-11-02;
Q1 and Q2 are RFC 0064's (§7 there).

- [x] **Q3. If recent logs can't be reached, should a query fail or
  return what it has? (B17)** **Both.** By default the query returns
  the older results marked incomplete (`tail_unavailable`,
  `parquet_unavailable`), so dashboards keep working with a warning; a
  request with `require_complete: true` fails instead, for alerting
  rules where a missing tail must not read as "nothing happened" (§6.7,
  RFC0065.14).
- [x] **Q4. Should Ourios block duplicates when a client resends after
  a lost acknowledgement? (H8)** **No; accepted and documented.** It is
  how OTLP behaves everywhere and how the local backend behaves today;
  replication adds no new duplicates of this kind, and its own are
  removed by position (§6.6). An Ourios-only request ID would be sent
  by almost no client, since OTLP has no such field; revisit if a real
  client asks.
- [x] **Q5. Can a running cluster add machines that accept writes?**
  **Not in this RFC.** A node with a changed voting layout refuses to
  start (§6.1); an in-place migration (flush to Parquet, then switch
  layouts) gets its own RFC. Tail capacity scales at any time (B24),
  and the 16 default groups spread load over up to 16 machines without
  a layout change.

- [ ] **Q6. How does a replicated group recover from a deleted or
  rolled-back high-water object?** RFC 0059 §3.1's quiesced
  re-bootstrap removes every root's seated marker. On a replicated root
  `log_ids.json` stays, so after a deletion no replicated start
  re-creates the object and each fails closed, and the rollback check
  reads each group's latest applied id block, in its log and snapshot
  record, which no operator step removes. So after a deletion every
  group, and after a rollback every group with a block above the
  object, appends no id block (§6.5) and its miner runs out of ids
  until a recovery is defined. That recovery must floor the new
  object above every block any group's log or bundle holds, not only
  above the published ids.

- [ ] **Q7. How does a forced takeover interact with checkpoint
  bundles, `checkpoint.json`, id blocks and template versions?** Not
  yet specified; tracked in #1227. `ourios wal force-leader` must
  not be implemented until that amendment lands. Known gaps:
  - `published_high`: which object holds it and when it is written,
    so that `force-leader` starts above every position ever published,
    including files published after the last checkpoint (B8).
  - A former flusher still running after the takeover can still write
    files, bundles and `checkpoint.json` under the same log id.
  - The survivor's uncommitted suffix and the new log share one
    position namespace.
  - A replica restored from an older bundle can resume an id range the
    lost suffix already used.
  - A returning replica purged past the survivor's base.
  - The `template_version` floor read from the audit trail can differ
    between tail replicas while a former flusher still publishes.

- [ ] **Q8. Can one store root serve both WAL backends?** Not yet;
  tracked in #1237. §6.5 makes a root serve one backend: local
  receivers and replicated groups on one root, or a root moved between
  them, are unsupported, so a local start bootstrapping the high-water
  object beside replicated groups, or after one was deleted there, is
  out of scope by statement. Local starts know nothing of
  `log_ids.json`, and a mixed root would need one bootstrap lock both
  backends honour.

Decided as technical calls: RFC0065.10's check uses `openraft`'s
upstream verification plus our seeded simulation suite, not a TLA+
specification of our own (§8 gives the reasoning).

## 10. References

- `AGENTS.md` §3.1–§3.7, §4; `docs/hazards.md` H3, H4, H6, H8
- RFC 0064 (the `meta:` prerequisite); RFC 0001 §6.9 (snapshot);
  RFC 0004 (classification); RFC 0005 §3.2, §3.4, §3.9 (amended, file
  names, reader contract); RFC 0008 §4, §6.2.2, §6.6, §6.7, §6.9
  (survey precedent, frame kinds, recovery, truncation, tunables);
  RFC 0009 §3.4, §3.5 (compare-and-swap; amended); RFC 0014 §3.2; RFC 0016 §3.4 and RFC 0027 (extended); RFC 0030 (TLS
  sections); RFC 0033 (template map); RFC 0038 §3.5 (spans); RFC 0046
  §3.3 (tenant frame); RFC 0047 (visibility); RFC 0052 §3.1 (barrier,
  checkpoint); RFC 0053 §3.1 (retryable rejection); RFC 0059 §3.6
  (several receivers); RFC 0063; RFC 0066 (Kafka backend)
- W. Lin, M. Yang, L. Zhang, L. Zhou, *PacificA: Replication in
  Log-Based Distributed Storage Systems*, Microsoft Research
  MSR-TR-2008-25
- L. Lamport, D. Malkhi, L. Zhou, *Vertical Paxos and Primary-Backup
  Replication*, PODC 2009
- D. Ongaro, J. Ousterhout, *In Search of an Understandable Consensus
  Algorithm*, USENIX ATC 2014 (log matching; read-index reads)
- Apache Kafka KIP-101, *Alter Replication Protocol to use Leader Epoch
  rather than High Watermark for Truncation*
- C. Gray, D. Cheriton, *Leases*, SOSP 1989; M. Kleppmann, *How to do
  distributed locking*, 2016 (fencing tokens)
- R. Alagappan et al., *Protocol-Aware Recovery for Consensus-Based
  Storage*, FAST 2018
- Jepsen analyses of Kafka, Redpanda and etcd
- M. H. Kristensen, T. Wright, C. Gomes, L. Esterle, P. G. Larsen,
  *DynSRV: Dynamically Updated Properties for Stream Runtime
  Verification*, RV 2025, pp. 101–119 (`defer` and `dynamic`; §5's
  history kept for a dynamically added property)
- J. Zhang, B. H. C. Cheng, *Using Temporal Logic to Specify Adaptive
  Program Semantics*, Journal of Systems and Software 79(10), 2006
  (one-point, guided and overlap adaptation)
- `openraft` 0.9.25 / 0.10.0-alpha.37 (databendlabs/openraft: README,
  `RaftLogStorage`, `docs/protocol/read.md`, `jepsen/`, `tests-turmoil/`,
  issues #1722, #1795, #1802, #1808, #2167, #2245); `raft` 0.7.0
  (tikv/raft-rs, issue #599; RUSTSEC-2024-0437); `omnipaxos` 0.2.4;
  SlateDB RFC 0001 (manifest fencing); `turmoil` 0.7.2 and `turmoil-fs`
  0.1.0
- Issue #1141
