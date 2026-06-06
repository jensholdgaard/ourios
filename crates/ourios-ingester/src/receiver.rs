//! OTLP receiver (RFC 0003, `red`).
//!
//! The ingest front door (OTLP logs over gRPC/HTTP), the Drain-derived
//! mining pipeline, and the WAL-before-ack durability path
//! (`CLAUDE.md` §3.4, RFC 0008) live here, grown one §8 group at a time
//! as the `tests/rfc0003_*` acceptance tests go green.
//!
//! Landed so far:
//! - [`decode`] — the §6.2 wire-decode layer (protobuf + OTLP/JSON),
//!   turning a request payload into an `ExportLogsServiceRequest`. The
//!   [`http`] and [`grpc`] transports hand their decoded payload to this
//!   shared layer.
//! - [`materialize`] — the §6.1 step 2–3 mapping from a decoded
//!   `LogRecord` to the flat `OtlpLogRecord` the miner consumes (body
//!   fork + empty-sentinel narrowing).
//! - [`tenant`] — per-`ResourceLogs` tenant derivation + the
//!   [`tenant::fan_out`] that tags each record with its `tenant_id`
//!   (RFC0003.3/.4).
//! - [`pipeline`] — the §6.5 WAL-before-ack ingest path
//!   ([`pipeline::IngestPipeline`]): fan out → append one `OtlpBatch`
//!   frame → fsync → miner → ack (RFC0003.1/.12).
//! - [`http`] — the OTLP/HTTP listener ([`http::router`]) wrapping the
//!   pipeline: `Content-Type`/`Content-Encoding` dispatch, controlled
//!   transport errors, configurable path (RFC0003.11 HTTP arms / .13 /
//!   .14).
//! - [`grpc`] — the OTLP/gRPC `LogsService` ([`grpc::LogsReceiver`])
//!   wrapping the same pipeline: controlled `Status` mapping + concurrent
//!   WAL-before-ack (RFC0003.11 gRPC arms / .15).

pub mod decode;
pub mod grpc;
pub mod http;
pub mod materialize;
pub mod pipeline;
pub mod tenant;

pub use decode::{DecodeError, decode_json, decode_protobuf};
pub use materialize::{materialize_record, materialize_resource_logs};
pub use pipeline::{IngestPipeline, Journal, ReceiveError, SharedPipeline};
pub use tenant::{TenantResolutionError, TenantRule, fan_out};
