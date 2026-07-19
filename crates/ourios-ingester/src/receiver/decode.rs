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
/// `#[non_exhaustive]`: future transports/encodings may add variants,
/// so downstream `match`es must keep a wildcard arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum DecodeError {
    /// Protobuf bytes failed `prost` decode — a malformed wire payload.
    Protobuf(prost::DecodeError),
    /// OTLP/JSON bytes failed `serde_json` decode — malformed JSON, or
    /// JSON that doesn't match the proto3-JSON mapping.
    Json(serde_json::Error),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protobuf(e) => write!(f, "OTLP protobuf decode failed: {e}"),
            Self::Json(e) => write!(f, "OTLP/JSON decode failed: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protobuf(e) => Some(e),
            Self::Json(e) => Some(e),
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

/// Decode an OTLP/JSON `ExportLogsServiceRequest` — the HTTP
/// `application/json` transport body, in the proto3-JSON mapping
/// (RFC0003.6). Unknown fields are ignored per the OTLP/JSON spec
/// (serde's default), and the proto3-JSON deviations — hex
/// `traceId`/`spanId`, integer enums, decimal-string 64-bit ints
/// accepted as number or string, base64 bytes, lowerCamelCase keys —
/// are handled by the proto types' `with-serde` deserialiser.
///
/// Parses through [`ourios_core::otlp::lenient_json`]: the upstream
/// `with-serde` deserialiser rejects proto3-JSON's valid encodings of
/// an UNSET `AnyValue` (`{}` / `null` — real exporters emit them for
/// empty-body events), which would 400 a spec-compliant client
/// (ourios#549). The lenient path is a failed-parse retry only; valid
/// input never leaves `serde_json`'s direct path.
///
/// The returned `bool` is `true` when only the lenient retry parsed
/// the payload — threaded to `ourios.ingest.batches`' \
/// `ourios.ingest.json.lenient` attribute (`CLAUDE.md` §6.3: the
/// operator signal for upstream-rejected-but-valid payloads, and the
/// shim-dormancy signal once the upstream fix ships).
///
/// # Errors
///
/// Returns [`DecodeError::Json`] if `bytes` is not well-formed OTLP/JSON
/// for an `ExportLogsServiceRequest`.
pub fn decode_json(bytes: &[u8]) -> Result<(ExportLogsServiceRequest, bool), DecodeError> {
    ourios_core::otlp::lenient_json::from_slice_flagged(bytes).map_err(DecodeError::Json)
}
