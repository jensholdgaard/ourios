//! `ourios-ingester` — the ingester role (`CLAUDE.md` §1, §7).
//!
//! The ingester accepts OTLP logs, mines templates, writes them
//! durably (WAL-before-ack), lands Parquet in object storage, and runs
//! background maintenance. The `ourios-server` binary serves its
//! listeners; this crate holds the pieces it wires together:
//!
//! - **OTLP receiver** (RFC 0003) — the gRPC/HTTP ingest front door +
//!   mining pipeline: [`receiver::decode`] (§6.2 wire decode — protobuf +
//!   OTLP/JSON), [`receiver::materialize`] (§6.1 `LogRecord` →
//!   `OtlpLogRecord`), [`receiver::selector`] and [`receiver::tenant`]
//!   (RFC 0046 — the tenant arrives out of band on the request and is
//!   never derived from the payload), [`receiver::pipeline`] (§6.5
//!   WAL-before-ack), [`receiver::commit`] (group-commit fsync),
//!   [`receiver::http`] (OTLP/HTTP listener), and [`receiver::grpc`]
//!   (OTLP/gRPC `LogsService`).
//! - **WAL-before-ack** (RFC 0008 / `CLAUDE.md` §3.4) — durability
//!   before acknowledgement, via `ourios-wal`. Wired into the ingest
//!   path by [`receiver::pipeline`]: every non-empty batch is appended +
//!   fsync'd before its ack (RFC0003.1). The startup side is
//!   [`recovery`] — the RFC 0008 §6.6 driver restoring per-tenant miner
//!   snapshots ([`snapshot_store`], RFC 0001 §6.9) and replaying the WAL
//!   under per-consumer suppression horizons (RFC0008.10).
//! - **Write path** — [`encode_pool`] (RFC 0035) encodes mined records
//!   off the ordered mining phase; [`record_sink`] (RFC 0014) buffers
//!   them per partition and flushes Parquet to the store; [`audit_sink`]
//!   buffers the miner's template audit events; and [`publish`] orders
//!   the two so no record becomes query-visible before its template's
//!   audit event is durable. The one exception is a permanent audit
//!   failure (malformed event dropped): its records still publish,
//!   degraded, and those templates render retained or empty.
//! - **Publication barrier** (RFC 0052) — [`barrier`] captures a cut
//!   (encodes quiesced, both sinks drained), flushes it, and stamps the
//!   WAL checkpoint; [`cadence`] holds the latch that decides whether a
//!   cut may stamp.
//! - **Authorization graph** (RFC 0047) — `graph_emitter` (behind the
//!   `openfga` feature) derives relationship tuples from stored rows and
//!   writes them to `OpenFGA`.
//! - **Background compaction** (RFC 0009) — [`compactor`] sweeps the
//!   store for sealed, candidate partitions
//!   ([`ourios_parquet::plan_candidates`]) and consolidates them
//!   ([`ourios_parquet::compact_partition`]); [`metrics`] carries its
//!   instruments.

#![deny(unsafe_code)]

pub mod audit_sink;
pub mod barrier;
pub mod cadence;
pub mod compactor;
pub mod encode_pool;
#[cfg(feature = "openfga")]
pub mod graph_emitter;
mod lane;
pub mod metrics;
pub mod publish;
pub mod publisher;
pub mod receiver;
pub mod record_sink;
pub mod recovery;
pub mod snapshot_store;

pub use compactor::{Compactor, IngestError, SweepReport, run_sweep, run_sweep_with_promoted};
pub use metrics::CompactionMetrics;
pub use recovery::{RecoveryDriverError, RecoveryReport, TenantRecovery};
pub use snapshot_store::SnapshotStoreError;
