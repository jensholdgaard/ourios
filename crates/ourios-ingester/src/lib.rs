//! `ourios-ingester` — the ingester role (`CLAUDE.md` §1, §7).
//!
//! The ingester accepts OTLP logs, mines templates, writes them
//! durably (WAL-before-ack), lands Parquet in object storage, and runs
//! background maintenance. It spans three RFCs at different maturities:
//!
//! - **OTLP receiver** (RFC 0003, `red`, greening) — the gRPC/HTTP
//!   ingest front door + mining pipeline. The §5 acceptance criteria
//!   (RFC0003.1–.15) are enumerated as `tests/rfc0003_*`; the green
//!   slices flip them one §8 group at a time. Landed: [`receiver::decode`]
//!   (§6.2 wire decode — protobuf + OTLP/JSON, RFC0003.5/.6),
//!   [`receiver::materialize`] (§6.1 `LogRecord` → `OtlpLogRecord`,
//!   RFC0003.7–.10), [`receiver::tenant`] (per-`ResourceLogs` tenant
//!   derivation + fan-out, RFC0003.3/.4), [`receiver::pipeline`]
//!   (§6.5 WAL-before-ack ingest path, RFC0003.1/.12), and
//!   [`receiver::http`] (the OTLP/HTTP listener, RFC0003.11-HTTP/.13/.14),
//!   and [`receiver::grpc`] (the OTLP/gRPC `LogsService`,
//!   RFC0003.11-gRPC/.15). Only crash-before-ack (RFC0003.2) remains.
//! - **WAL-before-ack** (RFC 0008 / `CLAUDE.md` §3.4) — durability
//!   before acknowledgement, via the shipped `ourios-wal`. Wired into
//!   the ingest path by [`receiver::pipeline`]: every non-empty batch is
//!   appended + fsync'd before its ack (RFC0003.1).
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

pub mod compactor;
pub mod metrics;
pub mod receiver;

pub use compactor::{Compactor, IngestError, SweepReport, run_sweep};
pub use metrics::CompactionMetrics;
