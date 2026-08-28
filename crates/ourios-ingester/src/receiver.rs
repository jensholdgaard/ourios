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
//! - [`selector`] — the RFC 0046 §3.1 out-of-band tenant selector
//!   (`X-Ourios-Tenant` / `x-ourios-tenant`), required once per export.
//! - [`tenant`] — [`tenant::assign`]: every record of the export under the
//!   selected tenant (RFC 0046 §3.2).
//! - [`pipeline`] — the §6.5 WAL-before-ack ingest path
//!   ([`pipeline::IngestPipeline`]): assign → append one `TenantOtlpBatch`
//!   frame → fsync → miner → ack (RFC0003.1/.12).
//! - [`commit`] — the RFC0008.8 group-commit coordinator
//!   ([`commit::CommitCoordinator`]): windowed batched fsync that folds
//!   N concurrent appends into one `sync` per `wal_batch_window_ms`
//!   (or until the segment fills) while keeping the §3.4 ack gate.
//! - [`http`] — the OTLP/HTTP listener ([`http::router`]) wrapping the
//!   pipeline: `Content-Type`/`Content-Encoding` dispatch, controlled
//!   transport errors, configurable path (RFC0003.11 HTTP arms / .13 /
//!   .14).
//! - [`grpc`] — the OTLP/gRPC `LogsService` ([`grpc::LogsReceiver`])
//!   wrapping the same pipeline: controlled `Status` mapping + concurrent
//!   WAL-before-ack (RFC0003.11 gRPC arms / .15).

pub mod commit;
pub mod decode;
pub mod grpc;
pub mod http;
pub mod materialize;
pub mod pipeline;
pub mod selector;
pub mod tenant;

// RFC 0051: the role-independent serving plumbing moved to
// `ourios-serving`. These shims keep the old paths compiling for one
// release (RFC0051.7); import from `ourios_serving` instead.
#[deprecated(
    since = "0.10.0",
    note = "moved to ourios-serving (RFC 0051); use ourios_serving::auth"
)]
pub mod auth {
    pub use ourios_serving::auth::*;
}
#[deprecated(
    since = "0.10.0",
    note = "moved to ourios-serving (RFC 0051); use ourios_serving::propagation"
)]
pub mod propagation {
    pub use ourios_serving::propagation::*;
}
#[deprecated(
    since = "0.10.0",
    note = "moved to ourios-serving (RFC 0051); use ourios_serving::tls"
)]
pub mod tls {
    pub use ourios_serving::tls::*;
}
#[deprecated(
    since = "0.10.0",
    note = "moved to ourios-serving (RFC 0051); use ourios_serving::tls_serve"
)]
pub mod tls_serve {
    pub use ourios_serving::tls_serve::*;
}

pub use commit::CommitCoordinator;
pub use decode::{DecodeError, decode_json, decode_protobuf};
pub use materialize::{materialize_record, materialize_resource_logs};
pub use ourios_serving::auth::{
    AuthBinding, AuthError, AuthResolver, GraphIdentity, authenticate_bearer,
};
pub use ourios_serving::propagation::{
    HeaderExtractor, MetadataExtractor, extract_context, extract_context_from_metadata,
};
pub use pipeline::{IngestPipeline, Journal, ReceiveError, SharedPipeline};
pub use selector::{SelectorError, TENANT_HEADER};
pub use tenant::assign;
