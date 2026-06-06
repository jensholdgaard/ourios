//! OTLP receiver (RFC 0003, `red`).
//!
//! The ingest front door (OTLP logs over gRPC/HTTP), the Drain-derived
//! mining pipeline, and the WAL-before-ack durability path
//! (`CLAUDE.md` §3.4, RFC 0008) live here, grown one §8 group at a time
//! as the `tests/rfc0003_*` acceptance tests go green.
//!
//! Landed so far:
//! - [`decode`] — the §6.2 wire-decode layer (protobuf today; OTLP/JSON
//!   next), turning a request payload into an
//!   `ExportLogsServiceRequest`. No live `tonic`/`axum` listener yet:
//!   the transports hand their decoded payload to this same layer, so
//!   decode is specified and tested before the framing is wired.

pub mod decode;

pub use decode::{DecodeError, decode_protobuf};
