//! `ourios-ingester` — the ingester role (`CLAUDE.md` §1, §7).
//!
//! The ingester accepts OTLP logs, mines templates, writes them
//! durably (WAL-before-ack), lands Parquet in object storage, and runs
//! background maintenance. It spans three RFCs at different maturities:
//!
//! - **OTLP receiver** (RFC 0003) — the gRPC/HTTP ingest front door +
//!   mining pipeline. **All §5 acceptance criteria (RFC0003.1–.15) are
//!   live** across [`receiver::decode`] (§6.2 wire decode — protobuf +
//!   OTLP/JSON), [`receiver::materialize`] (§6.1 `LogRecord` →
//!   `OtlpLogRecord`), [`receiver::tenant`] (per-`ResourceLogs` tenant
//!   derivation + fan-out), [`receiver::pipeline`] (§6.5 WAL-before-ack),
//!   [`receiver::http`] (OTLP/HTTP listener), and [`receiver::grpc`]
//!   (OTLP/gRPC `LogsService`). Not yet wired: a served-socket binary
//!   (§9 process-model — the listeners are exercised in-process).
//! - **WAL-before-ack** (RFC 0008 / `CLAUDE.md` §3.4) — durability
//!   before acknowledgement, via the shipped `ourios-wal`. Wired into
//!   the ingest path by [`receiver::pipeline`]: every non-empty batch is
//!   appended + fsync'd before its ack (RFC0003.1). The startup side is
//!   [`recovery`] — the RFC 0008 §6.6 driver restoring per-tenant miner
//!   snapshots ([`snapshot_store`], RFC 0001 §6.9) and replaying the WAL
//!   under per-consumer suppression horizons (RFC0008.10).
//! - **Background compaction** (RFC 0009 §3.2, `specified`) — the only
//!   subsystem implemented in this scaffold. [`compactor`] sweeps the
//!   store for sealed, candidate partitions
//!   ([`ourios_parquet::plan_candidates`]) and consolidates them
//!   ([`ourios_parquet::compact_partition`]).
//!
//! This is the **scaffold**: it establishes the crate as the ingester
//! home and lands the compaction runner. The receiver and WAL ack path
//! follow their own RFCs through the maturity ladder.

#![deny(unsafe_code)]

pub mod audit_sink;
pub mod compactor;
pub mod encode_pool;
pub mod metrics;
pub mod publish;
pub mod receiver;
pub mod record_sink;
pub mod recovery;
pub mod snapshot_store;

pub use compactor::{Compactor, IngestError, SweepReport, run_sweep, run_sweep_with_promoted};
pub use metrics::CompactionMetrics;
pub use recovery::{RecoveryDriverError, RecoveryReport, TenantRecovery};
pub use snapshot_store::SnapshotStoreError;
