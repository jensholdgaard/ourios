---
rfc: 0066
title: Kafka WAL backend and a queryable tail
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-11-01
supersedes: —
superseded-by: —
---

# RFC 0066 — Kafka WAL backend and a queryable tail

> **Status: `drafted`.** Tracks #1141 (option 2). It is the second
> replicated backend behind the seam **RFC 0065** (`drafted`, the
> built-in replicated WAL) defines, and reuses that RFC's
> `JournalPosition` and ingest-position columns (RFC 0065 §6.3, §6.6),
> and must meet the same required behaviour (RFC 0065 §3), including the
> queryable tail's (§3.5 there), except that §3.12 here replaces B1's
> fsync with the broker's in-sync acknowledgement; it cannot reach `red`
> before RFC 0065 does.
> A Kafka broker acknowledges on replication, not on fsync, which
> `AGENTS.md` §3.4 does not allow even after RFC 0064, so this RFC also
> needs a **further `meta:` RFC** widening "durably" to a broker's
> acknowledgement from at least two in-sync replicas before it can reach
> `red` (§3.12). It shares RFC 0065's amendments of RFC 0005 §3.2 and
> RFC 0009 §3.5 (compaction keeps one row per ingest position) and its
> extension of RFC 0016 §3.4, and adds a broker variant to the miner
> snapshot's high-water mark (RFC 0001 §6.9, `accepted`). It coordinates
> with RFC 0052 (`green`), whose barrier and checkpoint it reuses for
> the broker backend, RFC 0053 (`drafted`), whose retryable rejection
> it uses, and RFC 0063 (`green`). RFC 0008 stays the local backend's
> specification, unchanged.

## 1. Summary

The receiver's WAL gets a second backend: a topic on a Kafka-protocol
broker (`receiver.wal.backend: kafka`). A receiver acknowledges an
export once the broker has acknowledged its frame from every in-sync
replica. Each tenant's frames go to one partition, chosen by a fixed
hash, and each partition is mined by one receiver, chosen by its
StatefulSet ordinal. Checkpoints and miner snapshots move to object
storage, so a broker-backed receiver needs no persistent volume. The
partition's owner also publishes the rows it has mined but not yet
flushed to a second topic. The querier reads that tail and merges it
with Parquet, so a record is queryable about a second after it is
acked. Every row carries its log position, and that position is the
dedup key across redelivery, handoff and the flush boundary. The local
backend stays the default and does not change.

## 2. Motivation

### 2.1 Freshness

The querier reads Parquet only, so an acked record stays invisible until
its partition flushes: up to five and a half minutes on a quiet tenant
today (RFC 0014 §3.2, #1141), less with RFC 0063. Incident queries ("what
did this service log in the last minute") want seconds. Getting there
means querying records that are durable but not yet in Parquet. On the
local WAL they live on one receiver's disk and in its memory, reachable
only through that receiver. On a broker they live in a shared, ordered
log that a querier can read directly.

### 2.2 Stateless receivers

A receiver today is a StatefulSet replica with a WAL volume
(`deploy/helm/ourios/templates/receiver-statefulset.yaml`,
`volumeClaimTemplates`). Losing the volume loses every acked record not
yet in Parquet; resizing or moving it is an operator task. RFC 0052
exists because that volume filled up. A broker backend removes the
volume: durability is the broker's replication (§3.12).

### 2.3 One miner per tenant

RFC 0059 §3.6 notes that "nothing routes a tenant to one replica": with
`receiver.replicas > 1` every replica mines every tenant it receives, so
one tenant's templates are split across several miners. Partitioning by
tenant gives each tenant one miner at a time.

### 2.4 Why at this layer

The `Journal` trait (`ourios-ingester/src/receiver/pipeline.rs`) is the
seam the ack path already goes through, and RFC 0052's barrier already
defines "everything before this point is in Parquet". The broker backend
is a second implementation of both, not a second ingest pipeline.

## 3. Proposed design

### 3.1 Configuration

A new `receiver.wal` section (YAML, every leaf `${env:VAR}`-substitutable
like `ReceiverSection`; bare env under `OURIOS_RECEIVER_WAL_*`):

```yaml
receiver:
  wal_root: /var/lib/ourios/wal   # local backend only, as today
  wal:
    backend: kafka                # local (default) | kafka
    kafka:
      brokers: "${env:OURIOS_RECEIVER_WAL_KAFKA_BROKERS}"  # comma-separated host:port
      topic: ourios-wal           # raw frames
      tail_topic: ourios-tail     # mined rows (§3.8)
      produce_timeout_ms: 5000
      tls: { ... }                # client TLS; same leaves as RFC 0030's TlsSection
      sasl:
        mechanism: "${env:OURIOS_RECEIVER_WAL_KAFKA_SASL_MECHANISM:-}"  # PLAIN | SCRAM-SHA-256 | SCRAM-SHA-512
        username: "${env:OURIOS_RECEIVER_WAL_KAFKA_USERNAME:-}"
        password: "${env:OURIOS_RECEIVER_WAL_KAFKA_PASSWORD:-}"
```

- `backend: local` is today's behaviour and requires `wal_root`.
  `backend: kafka` requires `brokers` and `topic`, and ignores
  `wal_root`. Anything else is a startup error naming the field.
- Ourios does not create topics. At startup the receiver reads both
  topics' metadata and fails startup if either is missing, if
  `min.insync.replicas` is below 2 (§3.12), or if the partition
  count differs from the one recorded in the store (§3.3).
- The batch window (`wal_batch_window_ms`, RFC 0008 §6.9) keeps its
  meaning: the group-commit coordinator collects turns for up to that
  long and sends one produce request per partition.
- Classification under RFC 0004: `backend` is a deployment choice,
  `produce_timeout_ms` a Tunable (`100..=60_000`), the rest locations
  and credentials.

### 3.2 The client

The broker is reached over the Kafka wire protocol, which Apache Kafka,
Redpanda and WarpStream all speak.

- **`rskafka` 0.6.0** (influxdata, MIT OR Apache-2.0, MSRV 1.85; last
  commit 2026-10-29) is pure Rust on tokio. Its README describes it as
  "a minimal Kafka implementation for simple workloads that wish to use
  Kafka as a distributed write-ahead log", with "no support for offset
  tracking, consumer groups, transactions". It produces with `acks = -1`
  (all in-sync replicas) and is tested against Kafka and Redpanda.
  Offset tracking and assignment are exactly what Ourios does itself
  (§3.4, §3.5), so the missing features are not needed.
- **`rdkafka` 0.39.0** wraps librdkafka 2.12.1 through `rdkafka-sys`,
  built with CMake or linked statically or dynamically. It brings
  consumer groups and an idempotent producer, but adds a C toolchain to
  every build, C code to the `-static` and `-scratch` musl images, and a
  large FFI surface outside our `#![deny(unsafe_code)]` crates.

The backend uses `rskafka` behind a small `BrokerClient` trait (produce
to a partition, fetch from an offset, read metadata), so the tests can
use an in-memory broker and a later RFC can swap the client.
`rskafka`'s dependencies are permissively licensed and go through the
existing `cargo deny` check.

### 3.3 Partitioning and tenancy

- **One topic, shared by all tenants.** A topic per tenant would make
  topic count grow with tenants, which brokers handle badly.
- **A tenant's frames go to one partition**:
  `partition = xxh3_64(tenant_bytes) mod partition_count`. The hash and
  its seed are fixed by this RFC and pinned by golden values (RFC0066.4),
  because changing them moves tenants between partitions. The record
  key is the tenant id, for operators reading the topic.
- **Per-tenant order.** One partition is totally ordered, so the miner
  sees a tenant's frames in the order the broker acknowledged them, as
  it sees the local WAL's frames in append order today.
- **The partition count is recorded** in `<bucket>/wal/<topic>/topic.json`
  (topic, partition count, broker cluster id) the first time a receiver
  starts on the topic, with `put_if_absent` (RFC 0013). A later start
  that sees a different count or cluster id refuses to start. Changing
  the count is a migration this RFC does not define (§7).
- **The tenant travels in the frame**: the record value is the RFC 0046
  §3.3 `TenantOtlpBatch` payload, byte for byte. Every consumer reads
  the tenant from the frame, never from the partition, so partitions
  shared between tenants cannot mix them (`AGENTS.md` §3.7).

### 3.4 The ack path

Today `Journal::append_batch` returns the frame's own `WalOffset`
before `sync` makes it durable, and RFC 0052 §3.1 stores that offset as
`last_durable`. A broker assigns the offset only when the produce
succeeds, so the trait changes shape:

- `append_batch` returns a `FrameTicket`; `sync` resolves every ticket
  appended since the last sync to a `JournalPosition`.
- RFC 0065 §6.3's `JournalPosition` gains
  `Broker { partition: i32, offset: i64 }`. The local and replicated
  backends know their position at append, so their tickets resolve to
  it immediately and their behaviour is unchanged.
- The broker backend's `sync` sends one produce request per partition
  touched in the batch window and returns when every one has been
  acknowledged by all in-sync replicas.
- A produce that fails or times out fails every turn in the window
  with the retryable rejection RFC 0053 §3.1 defines (`UNAVAILABLE`,
  HTTP `503`); `NOT_ENOUGH_REPLICAS` is the common cause. Nothing is
  acked. The receiver does not retry a produce itself: a timed-out
  produce may have been written, and the client's own retry makes the
  same duplicate a lost ack makes on the local backend today.
- After the ack, the receiver does **not** mine the batch. Mining moves
  to the partition's owner (§3.5).

### 3.5 Ownership, mining and recovery

- **Assignment.** With `R = receiver.replicas`, the replica with
  StatefulSet ordinal `i` owns every partition `p` with `p mod R == i`.
  Every replica produces to any partition; only the owner consumes.
- **The owner's loop**, per owned partition: fetch frames from its
  position, decode, mine, and emit to the record sink exactly as
  recovery does today (`ourios-ingester/src/recovery.rs`, the
  `FrameSink` path), then publish the mined rows to the tail (§3.8).
  The sink's size and age triggers (RFC 0014 §3.2, RFC 0063) are
  unchanged.
- **Checkpoint.** RFC 0052's barrier runs per owned partition: cut at
  the last consumed offset, publish everything buffered, then write
  `<bucket>/wal/<topic>/<partition>/checkpoint.json` with that offset
  and the barrier's wall-clock time. Every frame below the checkpoint
  is in Parquet. Ourios never commits offsets to the broker.
- **Miner snapshots** move to `<bucket>/wal/<topic>/snapshots/<tenant>.snap`,
  written at the barrier as today (`recovery::write_folded_snapshots`).
  The snapshot's `wal_high_water` gains a broker variant
  `{ partition, offset }`, which is a new snapshot format version; a
  snapshot of an older version is discarded and the tenant fully
  replayed, as `ourios-miner/src/snapshot.rs` already does for any
  unreadable snapshot.
- **Recovery** on start or on taking over a partition: read the
  checkpoint `c` and the snapshots, fetch from the lower of `c` and the
  snapshots' high-water marks, and suppress per consumer as RFC 0008
  §6.6 does: Parquet emission below `c`, mining at or below each
  tenant's snapshot mark.
- **Handoff.** When `R` changes, a partition's old and new owner can
  both mine it for a while. That is no worse than today, where every
  replica mines every tenant: template ids stay disjoint through RFC
  0059 §3.6's reservations, and the duplicate rows are removed by
  position (§3.6). An owner lease is not required for correctness and
  is left out (§4).
- **Reclamation** is the broker's retention; Ourios deletes nothing.
  The journal's RFC 0052 rotation and housekeeping methods answer an
  explicit "not applicable" on this backend instead of the trait's
  fail-closed default, and `reclaim_state` reports the consumer lag.
- **Retention gap.** If a partition's log start offset is above its
  checkpoint, acked records were deleted before they reached Parquet.
  The owner records an error event and
  `ourios.ingest.batches{error.type="retention_gap"}`, then continues
  from the log start; stopping would not bring the records back. The
  docs require topic retention well above the barrier interval and
  recommend alerting on consumer lag.

### 3.6 Position as the dedup key (H8)

Every mined row carries its **ingest position**, in RFC 0065 §6.6's
column with the partition as the group and the offset as the sequence
number: `(partition, offset, index)`, where `index` is the
record's place in its frame. The position is assigned once by the log,
so a redelivered frame, a frame mined by two owners during a handoff,
and a row seen both in the tail and in Parquet all carry the same key.
No time window is involved (H8: "dedup MUST be content-keyed, never
time-windowed"); the position plays the role of H8's producer id plus
sequence number, with the log as the single producer of sequence
numbers.

- **Storage.** RFC 0065 §6.6's `OPTIONAL` `ingest_group`,
  `ingest_seq` and `ingest_index` columns (its amendment of RFC 0005
  §3.2), kept for the file's life as there. Kafka offsets are
  non-negative, so `offset` fits `ingest_seq` and `partition`
  `ingest_group` unchanged.
- **Compaction** (RFC 0009) keeps the first row of each
  `(tenant, ingest position)` within a partition and drops the rest.
- **Queries** dedup by it at the flush boundary (§3.8).

### 3.7 Producer identity

Each frame also carries two record headers, `ourios.producer` (the
receiver process's UUIDv7) and `ourios.seq` (a per-process counter).
They are not the dedup key. They let an operator trace a frame back to
the receiver that acked it, and they let a later RFC add an idempotent
producer without changing the frame.

### 3.8 The queryable tail

The querier cannot mine raw frames: template ids are assigned by the
owner's miner. So the owner publishes what it has mined.

- **The tail topic** has the same partition count and the same tenant
  hash as the WAL topic. After mining a frame, the owner produces one
  record to the tail: an Arrow IPC batch of the mined rows in the RFC
  0005 §3.2 schema, the ingest-position columns included. Headers carry the
  source offset range and the tenant.
- **Audit before record holds in the tail.** A tail batch is produced
  only after the template definitions its rows use are readable by the
  querier: the batch carries every template first used in it
  (`template_id`, `template_version`, tokens), in the shape the RFC 0033
  map holds. This is the read-side form of RFC 0008 §6.4's rule.
- **The querier's tail provider** is a DataFusion table beside the
  Parquet listing (`ourios-querier/src/file_set.rs`), for one tenant:
  1. read the partition's checkpoint `c` and its barrier time `t`;
  2. fetch the tail partition from the first record at or after `t`
     (a timestamp offset lookup), keep rows whose tenant is the query's
     tenant and whose source offset is at or above `c`;
  3. drop tail rows whose ingest position repeats an earlier tail row
     (an owner restart re-publishes from `c`);
  4. anti-join the remaining rows on the ingest position against the
     Parquet files of that tenant whose UUIDv7 name (RFC 0005 §3.4) is
     at or after `t`: those are the only files an age or size flush can
     have written since the checkpoint.
  The union of Parquet and that remainder holds every acked record once.
- **Bounded cost.** The tail spans one barrier interval at most (300 s,
  RFC 0052 §3.1), and the anti-join reads only the ingest-position columns from
  files written in that interval.
- **Visibility.** The tail rows go through the same plan as Parquet
  rows, so tenant predicates, conversation predicates and row masking
  (RFC 0047) apply unchanged. The DSL is unchanged (H6).
- **Freshness.** A record becomes queryable when its frame is mined and
  its tail batch is acknowledged: typically well under a second after
  the ack. RFC0066.7 states the test's allowance.
- **Retention.** The tail topic needs to hold at least two barrier
  intervals. It is derived data: replaying the WAL topic from the
  checkpoint rebuilds it.

### 3.9 Telemetry

- The client records the OTel messaging conventions:
  `messaging.client.operation.duration`,
  `messaging.client.sent.messages` and
  `messaging.client.consumed.messages`, with `messaging.system = kafka`,
  `messaging.destination.name`, `messaging.destination.partition.id`
  and, on failure, `error.type` (for example `NOT_ENOUGH_REPLICAS`).
  These are upstream names and stay out of the registry.
- Ourios-owned names go through the shared `ourios-semconv` registry:
  a consumer-lag gauge per owned partition, and H8's two detection
  signals, the dedup hit count (compaction and query) and
  `bytes_stored / bytes_received` per tenant. Their names are settled in
  the registry PR, checked against the OTel conventions first.
- `error.type = retention_gap` (§3.5) uses the existing
  `ourios.ingest.batches` counter.

### 3.10 Helm

- `receiver.wal.backend` is a chart value, because it changes the
  workload's shape: with `kafka` the StatefulSet keeps its stable
  ordinals (§3.5) but renders no `volumeClaimTemplates`.
- The broker settings are rendered as `${env:…:-}` references in the
  receiver config and set through `receiver.extraEnv` or a secret, as
  RFC 0061 §3.1 does for `max_request_bytes`.
- The querier needs the same broker settings to read the tail; the
  chart renders them into the querier's config too.

### 3.11 Invariants and hazards

- **§3.1–§3.3 (miner):** unchanged. Frames reach the miner whole and in
  per-tenant order; body retention and reconstruction do not depend on
  where the frame came from.
- **§3.4 WAL-before-ack:** per §3.12. An export is acked only
  after every in-sync replica has its frame; a broker below two in-sync
  replicas fails the produce, never degrades the ack.
- **§3.5 schema:** one optional column; old files read unchanged.
- **§3.6 object storage as truth:** checkpoints, snapshots and the
  recorded topic layout live in the store; the broker holds records only
  up to the WAL horizon.
- **§3.7 tenancy:** the tenant is in every frame and every tail batch;
  every consumer and the querier filter by it.
- **H3:** the produce batch window is the latency knob; RFC0066.3 is
  the backend's crash test.
- **H4:** unchanged; the owner flushes as a receiver does today.
- **H8:** live on this backend and met by §3.6.

### 3.12 The durability rule this backend needs

After RFC 0064, `AGENTS.md` §3.4 allows replication only in addition to
an fsynced WAL. A Kafka-protocol broker with `acks=all` and
`min.insync.replicas ≥ 2` survives losing any one broker, disk
included, but Apache Kafka acknowledges on replication, not on fsync, so
a power loss across every in-sync replica at once can lose acked
records; Redpanda fsyncs before it acknowledges by default. This backend
therefore needs a `meta:` RFC that adds a broker case to §3.4:
"durably" means the append was acknowledged by every in-sync replica,
with at least two in sync, and a broker that cannot meet that fails the
append rather than degrading the ack. The earlier draft of RFC 0064 held
that text; it moves to its own `meta:` RFC if this backend goes ahead.

## 4. Alternatives considered

**Query the receivers directly (Loki-style).** The querier asks every
receiver for its unflushed records over an internal RPC. It works on the
local backend, but the querier must find and reach every replica, decide
what a missing answer means, and the records still die with a lost
volume. The broker gives one place to read and removes the volume.

**Let the querier mine the raw tail.** Template ids come from the
owner's miner and its reservations (RFC 0059); a second miner would
assign different ids, and `count by template_id` would disagree between
tail and Parquet. Publishing the owner's mined rows avoids that.

**Consumer groups for assignment.** Kafka's group protocol rebalances
partitions automatically, but `rskafka` does not implement it, and the
StatefulSet ordinal already gives a stable, deterministic owner. Using
groups would mean `rdkafka` (§3.2).

**An owner lease in the store.** A compare-and-swap lease per partition
would keep two owners from mining at once during a handoff. Dedup by
position already makes the overlap harmless, so the lease would add a
liveness dependency on the store for no correctness gain.

**A topic per tenant.** Strict isolation at the broker, but topic count
grows with tenants and partition assignment becomes per topic. Tenancy
is enforced by the frame's tenant instead, as on the local WAL.

**The built-in replicated WAL (RFC 0065).** No external system and an
fsync on every replica, but receivers keep their disks, and a querier
reads the tail from each group's leader rather than from a shared log,
so the tail is unavailable during an election. The two backends sit behind one
seam; this one is for teams that already run Kafka.

**Other logs: NATS JetStream, Apache Pulsar.** Both can replicate and be
read by offset. The Kafka protocol covers the most deployments
(Kafka, Redpanda, WarpStream, and managed Kafka services), and one
protocol keeps the backend small.

**Dedup on a content hash.** H8 allows it, but two genuinely identical
records sent twice by a client would collapse into one. The log position
removes only copies of the same log entry.

## 5. Acceptance criteria

> **Scenario RFC0066.1 — the backend is configurable and checked at start**
> - **Given** `receiver.wal.backend` unset, `local`, `kafka` with
>   `brokers` and `topic`, `kafka` without `brokers`, and `s3`
> - **When** the server resolves its config
> - **Then** the first three resolve to local, local and kafka, and the
>   last two fail startup with an error naming the field
> - **And** with `kafka`, startup fails when a topic is missing, when
>   its `min.insync.replicas` is below 2, or when its partition count or
>   cluster id differs from `<bucket>/wal/<topic>/topic.json`

> **Scenario RFC0066.2 — an export is acked only after every in-sync replica has it**
> - **Given** a broker backend on a three-broker cluster with
>   `min.insync.replicas = 2`
> - **When** an export is sent with all brokers up, and again with two
>   brokers stopped
> - **Then** the first is acked and its frame is readable from the
>   topic, and the second gets the retryable rejection (gRPC
>   `UNAVAILABLE`, HTTP `503`) and no frame from it becomes readable as
>   acked

> **Scenario RFC0066.3 — crash recovery on the broker backend**
> - **Given** a broker-backed receiver ingesting a steady stream
> - **When** it is killed with SIGKILL mid-batch and restarted
> - **Then** every acked record is in Parquet after the restart, and no
>   ingest position appears twice in a query over the window

> **Scenario RFC0066.4 — one tenant, one partition, in order**
> - **Given** the tenant hash
> - **When** it runs on a fixed set of tenant ids
> - **Then** it returns the golden partition numbers committed with
>   this RFC's implementation, and the frames of one tenant are mined in
>   the order the broker acknowledged them

> **Scenario RFC0066.5 — redelivery and handoff do not duplicate rows**
> - **Given** a partition mined twice from the same checkpoint, once by
>   a restarted owner and once by two owners during a replica change
> - **When** compaction runs and a query counts the records
> - **Then** the count equals the number of acked records and the
>   compacted files hold no repeated `(tenant, ingest position)`
> - **And** the dedup hit count records the removed rows

> **Scenario RFC0066.6 — a retention gap is reported**
> - **Given** a partition whose log start offset has passed its
>   checkpoint
> - **When** its owner starts
> - **Then** it records `ourios.ingest.batches{error.type="retention_gap"}`
>   and an error event naming the partition and both offsets, and
>   continues from the log start

> **Scenario RFC0066.7 — the tail is queryable and counted once**
> - **Given** a broker backend with the age trigger at 300 s and a test
>   clock that controls flushes
> - **When** records are acked and the same `count` query runs before
>   any flush, after an age flush, and after the next barrier
> - **Then** every run returns the same count
> - **And** on the CI runner, an acked record is returned by a query
>   within 5 s (an allowance over the expected sub-second path)

> **Scenario RFC0066.8 — the tail keeps tenancy and visibility**
> - **Given** two tenants hashed to the same partition, and a masked
>   attribute under RFC 0047
> - **When** each tenant queries the tail
> - **Then** each sees only its own rows, and the masked attribute is
>   masked exactly as in Parquet results

> **Scenario RFC0066.9 — the local backend is unchanged**
> - **Given** `backend: local`
> - **When** the RFC 0008, RFC 0014 and RFC 0052 suites run
> - **Then** they pass unchanged, and written Parquet files have no
>   `ingest_*` columns

> **Scenario RFC0066.10 — the chart renders the broker shape**
> - **Given** `receiver.wal.backend: kafka` and the broker variables in
>   `receiver.extraEnv`
> - **When** the chart renders
> - **Then** the receiver StatefulSet has no `volumeClaimTemplates`, and
>   the receiver and querier configs carry the `${env:…:-}` references

## 6. Testing strategy

- RFC0066.1, .4: unit tests in `ourios-server/src/config` and next to
  the hash; golden values committed with the hash.
- RFC0066.5, .6, .7, .8: integration tests against an in-memory
  `BrokerClient` (§3.2), runnable locally. RFC0066.7's equal-count arm
  is also a `proptest` over random flush and barrier points.
- RFC0066.2, .3 and RFC0066.7's latency arm: CI-only container tests
  against Redpanda and Apache Kafka, beside the existing testcontainers
  jobs in `ci.yml`. There is no local container runtime, so these run
  only in CI. RFC0066.3 extends the WAL crash-recovery job to the broker
  backend, as RFC 0064 §3.2 requires for every backend.
- RFC0066.9: the existing suites, unchanged.
- RFC0066.10: `deploy/helm/render-tests.sh`.
- `criterion`: an ack-latency bench for the broker backend against the
  in-memory client, and a query bench for the tail merge, before
  `validated`.

## 7. Open questions

- [ ] Is two in-sync replicas the right floor for §3.12, or three, as
  most production Kafka guides say? Two is the minimum that survives a
  disk loss; three survives one broker down for maintenance while still
  acking.
- [ ] Should §3.12 also require broker-side fsync (Redpanda's default)
  to close the correlated-power-loss gap, at the cost of excluding a
  default Kafka setup?

- [ ] Changing the partition count moves tenants between partitions.
  Refuse (this draft), or define a migration: drain to a barrier on
  every partition, then switch?
- [ ] Should ingest and mining become separate roles (receivers only
  produce; miners consume), so they scale apart? This draft keeps both
  in the receiver.
- [ ] `rskafka` (this draft) or `rdkafka`? `rskafka` is maintained by
  one company that no longer uses it; a fork may become necessary.
- [ ] On a retention gap, continue from the log start (this draft) or
  stop mining that partition until an operator acts?
- [ ] WarpStream stores its log in object storage and acknowledges
  slower than a disk-backed broker. Is that latency acceptable as a
  supported target, or documented as "works, slower acks"?
- [ ] Should the tail topic be optional (`tail_topic` unset → broker WAL
  without the queryable tail)?

## 8. References

- `AGENTS.md` §3.1–§3.7, §4; `docs/hazards.md` H3, H4, H6, H8
- RFC 0064, RFC 0065 (the replicated WAL and its seam), and the
  `meta:` RFC §3.12 needs; RFC 0001 §6.9 (snapshot);
  RFC 0004 (classification); RFC 0005 §3.2, §3.4, §3.9 (amended,
  file names, reader contract); RFC 0008 §6.4, §6.6, §6.9 (audit
  barrier, recovery, tunables); RFC 0009 (compaction); RFC 0013
  (`put_if_absent`); RFC 0014 §3.2; RFC 0030 (TLS sections); RFC 0033
  (template map); RFC 0046 §3.3 (tenant frame); RFC 0047 (visibility);
  RFC 0052 §3.1 (barrier, checkpoint); RFC 0053 §3.1 (retryable
  rejection); RFC 0059 §3.6 (several receivers); RFC 0061 §3.1
  (`extraEnv`); RFC 0063
- OTel semantic conventions: messaging spans and metrics, Kafka
- `rskafka` 0.6.0 (README, `src/client/partition.rs` `acks: -1`);
  `rdkafka` 0.39.0 / `rdkafka-sys` 4.10.0+2.12.1
- Issue #1141
