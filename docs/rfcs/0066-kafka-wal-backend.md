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
broker (`receiver.wal.backend: kafka`), and a receiver acknowledges an
export once every in-sync broker replica holds its frame. Each tenant's
frames go to one partition by a fixed hash, each partition is mined by
one receiver, and checkpoints and miner snapshots live in object
storage, so a broker-backed receiver needs no persistent volume. Tail
replicas mine the same partitions with RFC 0065's deterministic miner
and serve the rows not yet in Parquet, so a record is queryable about a
second after it is acked. Every row carries its log position, the
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
      produce_timeout_ms: 5000
      max_backlog_bytes: "${env:OURIOS_RECEIVER_WAL_KAFKA_MAX_BACKLOG_BYTES}"  # per partition above its checkpoint, required (§3.5)
      tail_peers: "${env:OURIOS_RECEIVER_WAL_TAIL_PEERS}"  # tail pool: id=host:port,… (§3.8)
      tail_replicas: 1            # tail replicas per partition from the pool, 1..=tail_peers
      tail_clients: "${env:OURIOS_RECEIVER_WAL_TAIL_CLIENTS}"  # querier certificate identities allowed to call Tail (RFC 0065 §6.8)
      tls: { ... }                # client TLS; same leaves as RFC 0030's TlsSection
      sasl:
        mechanism: "${env:OURIOS_RECEIVER_WAL_KAFKA_SASL_MECHANISM:-}"  # PLAIN | SCRAM-SHA-256 | SCRAM-SHA-512
        username: "${env:OURIOS_RECEIVER_WAL_KAFKA_USERNAME:-}"
        password: "${env:OURIOS_RECEIVER_WAL_KAFKA_PASSWORD:-}"
```

- `backend: local` is today's behaviour and requires `wal_root`.
  `backend: kafka` requires `brokers` and `topic`, and ignores
  `wal_root`. Anything else is a startup error naming the field.
- Ourios does not create topics. At startup the receiver reads the
  topic's metadata and its effective configuration and fails startup
  if the topic is missing, if `min.insync.replicas` is below 2 (§3.12),
  if any partition has fewer replicas than the larger of 2 and
  `min.insync.replicas` (a replication factor of 1 would pass the
  in-sync check and then reject every produce),
  if `unclean.leader.election.enable` is `true` (it would let a lagging
  replica lead and drop acknowledged batches, against RFC 0065 B6 and
  B8), if `cleanup.policy` is anything but exactly `delete` (compaction
  would remove tenant-keyed batches before their checkpoint, leaving
  holes no log-start check sees), if `retention.ms` or
  `retention.bytes` is not unlimited (Ourios reclaims by checkpoint,
  §3.5), if `max.message.bytes` is below the largest record batch the
  receiver can produce (RFC 0061's request limit plus frame overhead,
  §3.4), or if the partition count or topic id differs from the one
  recorded in the store (§3.3).
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
to a partition, fetch from an offset, read metadata (including each
partition's current replica assignment and each broker's `rack`) and
topic configuration, read the cluster's finalized features
(ApiVersions v3+, KIP-584; on Apache Kafka this is how RFC 0067's ELR
check reads `eligible.leader.replicas.version`), delete records below
an offset), so the tests can use an
in-memory broker and a later RFC can swap the client. `rskafka` 0.6.0
does not cover all of it, checked against its source: its Metadata
request stops at version 4, whose response has no topic id (added in
version 10, KIP-516), it has no DescribeConfigs message, and its produce
retries internally (§3.4). The backend therefore carries a patch or fork
of `rskafka` adding Metadata v10+ for the topic id, DescribeConfigs for
§3.1's startup checks, the finalized features from ApiVersions if its
parser drops them, and a produce without internal retry, offered
upstream; until upstream takes them the fork is a maintained
dependency. A replacement client must provide the same five operations
before it is chosen (§7).
`rskafka`'s dependencies are permissively licensed and go through the
existing `cargo deny` check.

### 3.3 Partitioning and tenancy

- **One topic, shared by all tenants.** A topic per tenant would make
  topic count grow with tenants, which brokers handle badly.
- **A tenant's frames go to one partition**:
  `partition = xxh3_64(tenant_bytes) mod partition_count`, with the
  hash, seed (0) and byte encoding (the tenant id's bytes as the
  `TenantOtlpBatch` header carries them, RFC 0046 §3.3, no
  normalisation) exactly as RFC 0065 §6.1 fixes them, pinned by golden
  values (RFC0066.4), because changing them moves tenants between
  partitions. The record
  key is the tenant id, for operators reading the topic.
- **Per-tenant order.** One partition is totally ordered, and the
  ordering contract is **increasing partition offset**: the miner sees a
  tenant's frames in offset order, as it sees the local WAL's frames in
  append order today. Acknowledgements of concurrent produce requests
  can complete in any order, so acknowledgement order is not the
  contract.
- **The partition count is recorded** in `<bucket>/wal/<topic>/topic.json`
  (topic, topic id, partition count, broker cluster id) the first time
  a receiver starts on the topic, with `put_if_absent` (RFC 0013). A
  later start that sees a different count, cluster id or topic id (the
  topic was deleted and recreated) refuses to start before producing or
  consuming, so an old checkpoint is never applied to a new topic's
  offsets. Checkpoints and bundles are also keyed by the topic id
  (§3.5). Changing
  the count is a migration this RFC does not define (§7).
- **The tenant travels in the frame**: a data record's value is the RFC
  0046 §3.3 `TenantOtlpBatch` payload, byte for byte. Every consumer
  reads the tenant from the frame, never from the partition, so
  partitions shared between tenants cannot mix them (`AGENTS.md` §3.7).
- **Every record says what it is.** A record header `ourios.kind`
  names its kind and `ourios.v` its format version (`1`): `batch` (the
  data record above), and the control records `checkpoint` (cut, bundle
  key, owner), `published` (file key, offsets held, owner),
  `erasure` (tenant, predicate, as RFC 0065 §6.7) and `id_block` (the
  reserved range, owner), whose values are JSON objects
  with exactly those fields. Consumers process every kind in offset
  order: `batch` is mined, `checkpoint`, `published` and `erasure` are
  applied as §3.5 and §3.8 say, `id_block` feeds the allocator. A record with an
  unknown kind or a higher version stops consumption of that partition
  and reports it, rather than being skipped, since skipping a control
  record could make replicas mine differently.

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
  acked. A size rejection (`MESSAGE_TOO_LARGE`, `RECORD_LIST_TOO_LARGE`)
  is not an outage, since resending the same batch cannot succeed: the
  coordinator splits a window's frames into record batches under the
  topic's `max.message.bytes`, startup refuses a topic whose limit is
  below one maximal frame (§3.1), and if the limit is lowered while
  running, the turns whose frames no longer fit get RFC 0061's
  non-retryable size response (`RESOURCE_EXHAUSTED` without
  `RetryInfo`, HTTP `413`). Nothing retries a produce on the receiver's behalf: a
  timed-out produce may have been written, and a resend would create a
  second copy at a new offset, which position dedup cannot see. `rskafka`
  0.6.0's `PartitionClient::produce` retries I/O failures internally
  (`maybe_retry`) without idempotent sequencing, so the `BrokerClient`
  either sets its backoff to zero retries for produce or, if that cannot
  be configured, carries a small patch or fork that removes the retry;
  enabling Kafka's idempotent producer is the alternative if the client
  is replaced (§7). A lost produce response is then the same duplicate a
  lost ack makes on the local backend today, from the client's own
  retry, and nothing more (RFC0066.13).
- After the ack, the receiver does **not** mine the batch. Mining moves
  to the partition's owner (§3.5).

### 3.5 Ownership, mining and recovery

- **Assignment.** With `R = receiver.replicas`, the replica with
  StatefulSet ordinal `i` owns every partition `p` with `p mod R == i`.
  Every replica produces to any partition; only the owner consumes.
- **The owner's loop**, per owned partition: fetch frames from its
  position, decode, mine, and emit to the record sink exactly as
  recovery does today (`ourios-ingester/src/recovery.rs`, the
  `FrameSink` path). The owner is its partitions' flusher in RFC 0065
  §6.5's sense; tail replicas mine the same partitions for the tail
  (§3.8).
  The sink's size and age triggers (RFC 0014 §3.2, RFC 0063) are
  unchanged.
- **Checkpoint.** RFC 0052's barrier runs per owned partition: cut at
  `c`, the offset of the last frame it has mined (inclusive: the bundle
  covers every record at offsets `≤ c`, data and control alike, and
  nothing above), publish everything buffered, write RFC 0065
  §6.5's immutable cut-consistent bundle under
  `<bucket>/wal/<topic>/<topic_id>/<partition>/checkpoints/<offset>/`, then
  publish the checkpoint **monotonically**:
  `<bucket>/wal/<topic>/<topic_id>/<partition>/checkpoint.json` changes only by
  `put_if_match`, and a writer whose cut is not above the stored one
  abandons its write; and the owner produces a **checkpoint record**
  (the cut, the bundle's key) to the WAL partition itself. Every
  consumer applies checkpoint records in offset order and ignores any
  whose cut is not above the last it applied, so a delayed former owner
  can neither move the store's checkpoint back nor make a replica
  restore from a cut whose history is gone. Every frame at or below `c`
  is in Parquet. Replay after a restore starts at `c + 1`, never at the
  checkpoint record's own offset, so frames produced between the cut
  and that record are mined; the record is applied when replay reaches
  it. Ourios never commits offsets to the broker.
- **Snapshot and bundle formats** follow RFC 0065 §6.5: the snapshot's
  `wal_high_water` gains a broker variant `{ partition, offset }`, a
  bundle-format change ships a migration, and a consumer that finds
  neither a usable bundle nor the partition's complete history fails
  closed for that partition, since `DeleteRecords` has removed what a
  full replay would need.
- **Recovery** on start or on taking over a partition: read the
  checkpoint `c`, restore its bundle, whose snapshots are cut at `c`,
  and fetch from `c + 1`, the same rule as replay above. An older
  bundle cannot be used once `DeleteRecords(before = c + 1)` has
  removed the history it would need.
- **Handoff and template ids.** When `R` changes, a partition's old and
  new owner can both mine it for a while, and position dedup keeps only
  one copy of each row, so both copies must be identical, `template_id`
  included (RFC 0065 B11). Disjoint ids from RFC 0059's per-process
  reservations would not be. So this backend uses RFC 0065 §6.7's
  log-ordered allocation: an owner that needs ids produces an
  "id block reserved" record (reserved in the store first, as today) to
  the WAL partition itself, every miner of that partition allocates
  from those records in offset order, and the checkpoint carries the
  cut-consistent miner and allocator bundle of RFC 0065 §6.5. Two
  owners replaying one partition from one checkpoint then mine
  identical rows, whichever copy survives. An owner lease is still not
  required for correctness and is left out (§4).
- **Reclamation is checkpoint-controlled (RFC 0065 B25).** Time and
  size retention are off (startup checks it), so the broker never
  deletes a frame on its own. After a checkpoint record for cut `c`, the owner
  deletes the partition's records at offsets `≤ c` (`DeleteRecords`
  with `before = c + 1`); the checkpoint record itself, above `c`,
  survives. During
  a long object-store outage the checkpoint cannot advance, so nothing
  is deleted, and once a partition's bytes above its checkpoint reach
  `max_backlog_bytes` further exports to it get the retryable
  rejection (B3), never a silent deletion. The limit is set explicitly
  (no default): the Kafka protocol gives a client no reliable view of a
  broker's free capacity shared with other topics, so the operator sizes
  it from the brokers' disks as the chart sizes RFC 0065's WAL volume,
  and a change takes effect at the next start. The journal's RFC
  0052 rotation and housekeeping methods answer an explicit "not
  applicable" on this backend, and `reclaim_state` reports the bytes
  above the checkpoint.
- **Retention gap** is then a misconfiguration: if a partition's log
  start offset is above `c + 1` for its applied checkpoint `c`, the
  consumer first re-reads `checkpoint.json`, since normal reclamation
  overtakes a consumer that lagged behind a newer checkpoint: if it
  names a cut `c'` with log start ≤ `c' + 1`, the consumer restores that
  bundle and continues from `c' + 1`; if the store is unreachable, it
  reports the tail unavailable (B17) and retries, skipping nothing.
  Only a log start above the newest checkpoint's `c' + 1` means someone
  changed retention or deleted records. The owner records an error event and
  `ourios.ingest.batches{error.type="retention_gap"}`, refuses to
  advance past the gap until an operator acknowledges it with an
  explicit CLI verb, and reports the tail unavailable for that range.

### 3.6 Position as the dedup key (H8)

Every mined row carries its **ingest position**, in RFC 0065 §6.6's
columns: `(log_id, offset, index)`, where `log_id` (stored in
`ingest_group`) is the log id allocated once per (topic id, partition),
never the raw partition number, `offset` (in `ingest_seq`) is the
record's partition offset, and `index` is the record's place in its
frame. The position is assigned once by the log,
so a redelivered frame, a frame mined by two owners during a handoff,
and a row seen both in the tail and in Parquet all carry the same key.
No time window is involved (H8: "dedup MUST be content-keyed, never
time-windowed"); the position plays the role of H8's producer id plus
sequence number, with the log as the single producer of sequence
numbers.

- **Storage.** RFC 0065 §6.6's `OPTIONAL` `ingest_group`,
  `ingest_seq` and `ingest_index` columns (its amendment of RFC 0005
  §3.2), kept for the file's life as there. `ingest_group` holds the
  log id RFC 0065 §6.6 allocates once per (topic UUID, partition) and
  records in `topic.json`, so a recreated or different topic, or a
  tenant moved from the replicated backend, never reuses a position
  already stored in Parquet; Kafka offsets are non-negative and fit
  `ingest_seq`.
- **Compaction** (RFC 0009) keeps the first row of each
  `(tenant, ingest position)` within a partition and drops the rest.
- **Queries** dedup by it across the whole input (§3.8).

### 3.7 Producer identity

Each frame also carries two record headers, `ourios.producer` (the
receiver process's UUIDv7) and `ourios.seq` (a per-process counter).
They are not the dedup key. They let an operator trace a frame back to
the receiver that acked it, and they let a later RFC add an idempotent
producer without changing the frame.

### 3.8 The queryable tail

The tail is served exactly as RFC 0065 §6.7 serves it, with the WAL
partition in place of the Raft log; there is no separate tail topic.

- **Tail replicas** are a pool of receivers in tail mode, set under
  `receiver.wal.kafka` (§3.1): `tail_peers` (`OURIOS_RECEIVER_WAL_TAIL_PEERS`,
  `id=host:port,…`) and `tail_replicas`
  (`OURIOS_RECEIVER_WAL_KAFKA_TAIL_REPLICAS`, `1..=tail_peers`), validated
  as RFC 0065 §3.8 validates its fields, and a node in `tail_peers` runs
  in tail mode. Each partition's tail replicas are chosen by rendezvous
  hashing as in RFC 0065 §6.1. Each restores the latest checkpoint's
  bundle, consumes its partitions from `c + 1` (§3.5), mines them with RFC 0065's deterministic miner and the
  log-ordered id allocation of §3.5, and keeps the mined rows above the
  checkpoint in memory and in local Parquet files, written by the same
  writer as the store's. Those files are a cache on an ephemeral volume,
  rebuilt by re-consuming the partition, so the receivers stay
  stateless. They hold only rows above the checkpoint, which
  `max_backlog_bytes` bounds per partition, so the chart sizes the
  volume for `max_backlog_bytes` times the topic's partition count, the
  bound for a node holding every partition, which no placement or
  reassignment can exceed, as RFC 0065 §6.7 does; a full volume is handled as RFC 0065 §6.7 handles a
  full tail volume.
- **Read barrier (B14).** A `Tail` call reads the partition's
  high-water offset `h` (`ListOffsets`) when it arrives and answers once
  the replica has mined through `h − 1`, or reports the tail
  unavailable past a bound (B17).
- **Cost (B19).** The replica prunes its local files by footer
  statistics and returns only the query's tenant, range and predicates,
  so a narrow query reads what it selects, not the backlog.
- **Published rows are served from the store only.** After each file it
  publishes, the owner produces a **published record** (the file's key
  and the offsets it holds) to the WAL partition, as it does checkpoint
  records, and consumers ignore one from an owner other than the one
  the latest checkpoint or assignment names. A tail replica that
  applies it stops serving those rows and returns the key to `Tail`
  callers until the checkpoint passes it, so an erasure (RFC 0047 §3.6)
  that rewrites a published file is never undone by a cached copy, as
  in RFC 0065 §6.7. The querier treats those keys as hints and
  re-resolves them through the current manifest, and templates are
  exchanged per query, as there.
- **Erasure goes through the partition.** As in RFC 0065 §6.7, before
  its confirming pass the compactor produces an **erasure record** (the
  tenant and the erasure's predicate) to every partition the tenant
  maps to, and waits for each produce to be acknowledged. A consumer
  that has applied it serves none of the matching rows below its
  offset, and an owner that has applied it writes none of them to a
  file it publishes afterwards, so no cached tail copy outlives the
  erasure. A broker acknowledgement does not prove that every owner has
  consumed the record, so an owner that lags or paused can still
  publish matching pre-request rows after the confirming pass: RFC 0009
  §7 D3's accepted residual, unchanged in scope, as in RFC 0065 §6.7,
  and no tail replica serves those rows.
- **Checkpoint without the store.** The replica holds the last
  checkpoint record it applied, from the WAL partition, so it answers a
  querier that cannot reach the store, including one that started
  during the outage; unreachable historical Parquet is reported as
  `parquet_unavailable`, as in RFC 0065 §6.7.
- **The querier's merge** is RFC 0065 §6.7's: Parquet listing, `Tail`
  from the checkpoint, then dedup by ingest position across the whole
  input, with candidates chosen by overlapping position ranges in
  footer statistics, never by file name or time.
- **A lost tail replica** loses only a cache: its replacement restores
  the bundle for the checkpoint `c`, consumes from `c + 1` as recovery
  does (§3.5) and serves once it has mined through the high-water. That needs the checkpoint's bundle, which is in the
  object store, and the records at or below the cut are deleted, so a
  tail replica that starts with an empty cache **during** a store
  outage cannot rebuild its miner state: it reports the tail
  unavailable (B17) until the store is reachable. RFC0066.11's
  guarantee covers tail replicas running when the outage began;
  `tail_replicas ≥ 2` keeps one through a restart.

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
- RFC 0067 §3.5's durability check: its outcome per run (verified,
  unverified, failed) and the topic's current mode, as Ourios-owned
  names through the registry; the re-check interval is tuned from it.

### 3.10 Helm

- `receiver.wal.backend` is a chart value, because it changes the
  workload's shape: with `kafka` the StatefulSet keeps its stable
  ordinals (§3.5) but renders no `volumeClaimTemplates`.
- The broker settings are rendered as `${env:…:-}` references in the
  receiver config and set through `receiver.extraEnv` or a secret, as
  RFC 0061 §3.1 does for `max_request_bytes`.
- Tail replicas are a second StatefulSet, `-wal-tail`, with an
  ephemeral volume for their local Parquet cache, rendered into
  `tail_peers`; it carries the broker settings, and queriers reach it
  over the peer protocol with mutual TLS, accepted only from the
  identities in `tail_clients` (RFC 0065 §6.8), so queriers need no
  broker settings.

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
- **§3.7 tenancy:** the tenant is in every frame and every tail row;
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
append rather than degrading the ack. That `meta:` RFC is **RFC 0067**
(`drafted`), which also decides how a broker that does not fsync first
is allowed; its runtime rules (an opt-in
`receiver.wal.kafka.allow_unverified_ack`, default `false`, and a
re-check every 60 s, RFC 0067 §3.5) are criteria RFC0066.16–.20 here.

## 4. Alternatives considered

**Query the receivers directly (Loki-style).** The querier asks every
receiver for its unflushed records over an internal RPC. It works on the
local backend, but the querier must find and reach every replica, decide
what a missing answer means, and the records still die with a lost
volume. The broker gives one place to read and removes the volume.

**A tail topic of mined rows.** The earlier draft had the owner produce
its mined rows to a second topic. It needed its own retention, a
source-to-tail offset mapping kept in step with every checkpoint and
rebuild, and every tail read scanned the whole partition's backlog
before filtering. Deterministic tail replicas (§3.8) need none of that,
and mined rows would cost 3.3–6.8× the raw replication traffic (RFC
0065 §7).

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
fsync on every replica, but receivers keep their disks. Both backends
serve the tail the same way (§3.8) behind one seam; this one is for
teams that already run Kafka.

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
>   its `min.insync.replicas` is below 2, its effective
>   `unclean.leader.election.enable` is `true`, its `cleanup.policy` is
>   not exactly `delete` (a compacted topic is refused), its retention
>   is not unlimited, its `max.message.bytes` is below one maximal
>   frame, any of its partitions has fewer replicas than
>   `max(2, min.insync.replicas)`, or its partition count, cluster id
>   or topic id differs from `<bucket>/wal/<topic>/topic.json` (a
>   recreated topic is refused)

> **Scenario RFC0066.2 — an export is acked only after every in-sync replica has it**
> - **Given** a broker backend on a three-broker cluster with
>   `min.insync.replicas = 2`
> - **When** an export is sent with all brokers up, and again with two
>   brokers stopped
> - **Then** the first is acked and its frame is readable from the
>   topic, and the second gets the retryable rejection (gRPC
>   `UNAVAILABLE`, HTTP `503`) and no successful acknowledgement; its
>   outcome is uncertain (§3.4), so its frame may become readable later
>   once the brokers return, at most once and without a client
>   retry

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
>   increasing offset order, including when two produce requests to the
>   partition complete out of order

> **Scenario RFC0066.5 — redelivery and handoff do not duplicate rows**
> - **Given** a partition mined twice from the same checkpoint, once by
>   a restarted owner and once by two owners during a replica change
> - **When** compaction runs and a query counts the records
> - **Then** the count equals the number of acked records and the
>   compacted files hold no repeated `(tenant, ingest position)`
> - **And** the dedup hit count records the removed rows

> **Scenario RFC0066.6 — a retention gap is reported, normal reclamation is not**
> - **Given** a tail replica that applied checkpoint `c` and lagged
>   while the owner checkpointed `c'` and deleted through `c'`, so the
>   log start is above `c + 1` but at most `c' + 1`
> - **When** the replica resumes
> - **Then** it restores the bundle for `c'` and continues from
>   `c' + 1` with no error event and no operator action; with the store
>   unreachable it reports its tail unavailable and retries, skipping
>   nothing
> - **And given** a partition whose log start is above `c' + 1` for the
>   newest checkpoint `c'`
> - **When** its owner starts
> - **Then** it records `ourios.ingest.batches{error.type="retention_gap"}`
>   and an error event naming the partition and both offsets, stops
>   consuming that partition, and reports its tail unavailable
> - **And** it continues from the log start only after an operator
>   acknowledges the gap with the CLI verb (§3.5)

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
> - **And** a separate `-wal-tail` StatefulSet renders with no
>   `volumeClaimTemplates`, an ephemeral volume for its Parquet cache
>   sized as §3.8 says, `tail_peers` and `tail_clients` in its config,
>   and the broker settings

> **Scenario RFC0066.11 — a prolonged outage stays complete and searchable (RFC 0065 B19, B25)**
> - **Given** a broker backend whose object store is unreachable for
>   longer than ten barrier intervals, at least one tail replica of
>   each partition running since before the outage, and a querier
>   started during the outage
> - **When** a narrow query and a broad query cover the outage
> - **Then** both return every record acknowledged during the outage,
>   and name the Parquet published before it (including files
>   published above the checkpoint) as `parquet_unavailable`; the
>   narrow one reads a number of bytes that does not grow with the
>   backlog, no frame
>   above the checkpoint is deleted, and once `max_backlog_bytes` is
>   reached exports get the retryable rejection

> **Scenario RFC0066.12 — a delayed former owner cannot move a checkpoint back**
> - **Given** a partition whose new owner has checkpointed cut 200 and
>   deleted the records below it
> - **When** the former owner, delayed, publishes its checkpoint for
>   cut 100 to the store and the partition
> - **Then** `checkpoint.json` still names cut 200, every consumer
>   ignores the cut-100 record, and a restarting tail replica restores
>   from cut 200

> **Scenario RFC0066.13 — a lost produce response makes no second copy**
> - **Given** a broker that appends a produce request and then drops its
>   response
> - **When** the receiver's produce times out
> - **Then** the export gets the retryable rejection, the broker holds
>   exactly one copy of the frame, and the client did not resend it

> **Scenario RFC0066.14 — size rejections are not outages, and erased rows stay erased**
> - **Given** a topic whose `max.message.bytes` is lowered below a
>   frame while the receiver runs, and separately a conversation whose
>   rows the owner has published above the checkpoint, with the owner
>   paused after writing one of its files and before producing that
>   file's published record
> - **When** an export with that frame arrives, and an erasure of the
>   conversation completes and the owner resumes before the tenant is
>   queried
> - **Then** the export gets the non-retryable size response
>   (`RESOURCE_EXHAUSTED` without `RetryInfo`, HTTP `413`), not
>   `UNAVAILABLE`; and the query returns no erased row from any tail
>   replica

> **Scenario RFC0066.15 — an unknown record stops the partition**
> - **Given** a partition holding a record whose `ourios.kind` is not
>   one of `batch`, `checkpoint`, `published`, `erasure` or `id_block`,
>   and separately one whose `ourios.v` is `2`
> - **When** an owner or tail replica consumes up to it
> - **Then** it stops consuming that partition at that offset, records
>   an error event naming the partition, offset, kind and version, and
>   reports its tail unavailable
> - **And** it mines no record past that offset, so no replica skips a
>   control record another one applied

> **Scenario RFC0066.16 — an unverified broker without the opt-in refuses to start (RFC 0067)**
> - **Given** a topic that passes RFC0066.1's checks, with
>   `flush.messages` unset and no `write.caching` key
> - **When** the receiver starts without
>   `receiver.wal.kafka.allow_unverified_ack`
> - **Then** startup fails before producing or consuming, with an error
>   naming the opt-in field and the topic keys that would verify
>   persistence
> - **And** when the topic configuration cannot be read at all, startup
>   fails whether or not the opt-in is set

> **Scenario RFC0066.17 — the opt-in starts with a warning (RFC 0067)**
> - **Given** the topic of RFC0066.16 on Apache Kafka, every partition's
>   replicas on brokers with distinct non-empty racks, finalized features
>   with `eligible.leader.replicas.version ≥ 1`, and the opt-in set
> - **When** the receiver starts
> - **Then** it starts, logs one WARN event naming the topic and stating
>   that acknowledged batches are not verified on disk, and reports the
>   unverified mode in its telemetry (§3.9)
> - **And** the same fixture with ELR disabled, or with the finalized
>   features unreadable, fails startup naming the ELR feature
> - **And** a topic whose replica floor or leader safety cannot be read
>   (no readable `min.insync.replicas` or unclean-election setting, as on
>   WarpStream) fails startup with the opt-in set

> **Scenario RFC0066.18 — the opt-in still needs separate failure domains (RFC 0067)**
> - **Given** the opt-in set, and a topic where one partition has two
>   replicas on brokers with the same rack, or a replica on a broker
>   with no rack
> - **When** the receiver starts
> - **Then** startup fails naming the partition and the rack

> **Scenario RFC0066.19 — a verified broker needs no opt-in (RFC 0067)**
> - **Given** a topic with `write.caching=false`, and separately one
>   with `flush.messages=1`, both passing RFC0066.1's checks
> - **When** the receiver starts without the opt-in
> - **Then** it starts with no warning and reports the verified mode

> **Scenario RFC0066.20 — a change while running fails closed (RFC 0067)**
> - **Given** a running receiver on a verified topic without the opt-in
> - **When** the topic's `flush.messages` is reset (or `write.caching`
>   set to `true`) and the next re-check runs
> - **Then** exports to that topic get the retryable rejection (gRPC
>   `UNAVAILABLE`, HTTP `503`) and none is acknowledged until a re-check
>   finds the topic verified again
> - **And** with the opt-in set, the same change keeps
>   acknowledgements flowing and emits a WARN event naming the topic,
>   provided the failure-domain and (on Apache Kafka) ELR checks pass on
>   entering unverified mode; if either fails then, exports get the
>   retryable rejection instead
> - **And** a re-check that finds `min.insync.replicas` below 2,
>   unclean leader election enabled, or the configuration unreadable
>   stops acknowledgements on that topic whether or not the opt-in is set
> - **And** in unverified mode with the opt-in set, a re-check that
>   cannot read the current replica assignment, finds two replicas of a
>   partition on one rack or a replica with no rack (a reassignment or a
>   relabelled broker), or on Apache Kafka finds ELR disabled or the
>   finalized features unreadable, stops acknowledgements on that topic
>   with the retryable rejection until a re-check passes

## 6. Testing strategy

- RFC0066.1, .4: unit tests in `ourios-server/src/config` and next to
  the hash; golden values committed with the hash.
- RFC0066.16–.20: unit tests against the in-memory `BrokerClient`,
  which returns the topic configuration, replica assignment, broker
  racks and finalized features each scenario
  names and changes them between re-checks; the re-check runs on a test
  clock.
- RFC0066.5, .6, .7, .8, .11, .12, .13, .14, .15: integration tests against an in-memory
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
- [ ] WarpStream stores its log in object storage and acknowledges
  slower than a disk-backed broker. Is that latency acceptable as a
  supported target, or documented as "works, slower acks"?

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
