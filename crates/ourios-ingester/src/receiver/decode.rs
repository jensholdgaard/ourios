//! OTLP wire decode (RFC 0003 §6.2).
//!
//! Turns a request payload into the in-memory
//! `ExportLogsServiceRequest` the business-logic layer fans out. Both
//! the gRPC and the HTTP `application/x-protobuf` transports deliver
//! the *same* protobuf payload, so they share [`decode_protobuf`]
//! (RFC0003.5); the HTTP `application/json` path's `decode_json`
//! (RFC0003.6) lands next and produces the same type.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use prost::Message;

/// Failure decoding an OTLP `ExportLogsServiceRequest` off the wire.
///
/// A transport handler maps this to the controlled transport-level
/// error RFC0003.11 requires (gRPC `INVALID_ARGUMENT` / HTTP 400) —
/// never a panic.
///
/// `#[non_exhaustive]`: the OTLP/JSON path (RFC0003.6) adds a `Json`
/// variant in the next slice, so downstream `match`es must keep a
/// wildcard arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum DecodeError {
    /// Protobuf bytes failed `prost` decode — a malformed wire payload.
    Protobuf(prost::DecodeError),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protobuf(e) => write!(f, "OTLP protobuf decode failed: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protobuf(e) => Some(e),
        }
    }
}

/// Decode an OTLP/protobuf `ExportLogsServiceRequest` — the payload
/// carried by both the gRPC and the HTTP `application/x-protobuf`
/// transport (RFC0003.5). Decode is transport-agnostic: a byte-equal
/// payload yields an equal request regardless of which transport
/// delivered it.
///
/// # Errors
///
/// Returns [`DecodeError::Protobuf`] if `bytes` is not a well-formed
/// `ExportLogsServiceRequest` protobuf message.
pub fn decode_protobuf(bytes: &[u8]) -> Result<ExportLogsServiceRequest, DecodeError> {
    ExportLogsServiceRequest::decode(bytes).map_err(DecodeError::Protobuf)
}
