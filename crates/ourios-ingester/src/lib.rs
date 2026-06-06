//! `ourios-ingester` — the ingester role (`CLAUDE.md` §1, §7).
//!
//! The ingester accepts OTLP logs, mines templates, writes them
//! durably (WAL-before-ack), lands Parquet in object storage, and runs
//! background maintenance. It spans three RFCs at different maturities:
//!
//! - **OTLP receiver** (RFC 0003, `red`, greening) — the gRPC/HTTP
//!   ingest front door + mining pipeline. The §5 acceptance criteria
//!   (RFC0003.1–.15) are enumerated as `tests/rfc0003_*`; the green
//!   slices flip them one §8 group at a time. [`receiver::decode`] is
//!   the first to land — the §6.2 wire-decode layer (RFC0003.5) — with
//!   tenant fan-out, transports, and the WAL-before-ack path to follow.
//! - **WAL-before-ack** (RFC 0008 / `CLAUDE.md` §3.4) — durability
//!   before acknowledgement, via the shipped `ourios-wal`. Wired into
//!   the ingest path once the receiver lands; not exercised here.
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
