//! OTLP/HTTP listener (RFC 0003 §6.2).
//!
//! An `axum` `POST` handler at a configurable path (default `/v1/logs`)
//! that decodes the body per `Content-Type` (`application/x-protobuf` or
//! `application/json`) and `Content-Encoding` (`identity` or `gzip`),
//! hands the decoded `ExportLogsServiceRequest` to the [`IngestPipeline`]
//! (WAL-before-ack), and returns an `ExportLogsServiceResponse`.
//!
//! Transport errors are controlled (RFC0003.11): unsupported media type
//! / encoding → 415, malformed body → 400, oversize → 413, an
//! unconfigured path → 404, tenant-resolution failure → 400. No panics.
//!
//! The pipeline is shared behind a `Mutex` — the WAL is a single writer
//! (RFC 0008 §3.1), so concurrent requests serialize on it. The lock is
//! never held across an `.await`, so a plain `std::sync::Mutex` suffices.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceResponse;
use prost::Message;

use crate::receiver::decode::{decode_json, decode_protobuf};
use crate::receiver::pipeline::{IngestPipeline, ReceiveError};

/// The ingest pipeline shared across requests. The single-writer WAL
/// forces serialization; concurrent requests queue on the mutex.
pub type SharedPipeline = Arc<Mutex<IngestPipeline>>;

/// OTLP/HTTP listener configuration.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The accepted `POST` path (default `/v1/logs`; configurable per
    /// §6.2 / RFC0003.14).
    pub path: String,
    /// Maximum request body size in bytes; a larger body is rejected with
    /// 413 (RFC0003.11).
    pub max_body_bytes: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            path: "/v1/logs".to_owned(),
            max_body_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Build the OTLP/HTTP router over `pipeline`.
pub fn router(pipeline: SharedPipeline, config: &HttpConfig) -> Router {
    Router::new()
        .route(&config.path, post(handle_logs))
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        .with_state(pipeline)
}

/// The OTLP wire format selected by `Content-Type`.
#[derive(Clone, Copy)]
enum WireFormat {
    Protobuf,
    Json,
}

/// The supported request `Content-Encoding`s.
enum Encoding {
    Identity,
    Gzip,
}

async fn handle_logs(
    State(pipeline): State<SharedPipeline>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(format) = content_type(&headers) else {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    };
    let raw = match content_encoding(&headers) {
        Some(Encoding::Identity) => body.to_vec(),
        Some(Encoding::Gzip) => match gunzip(&body) {
            Ok(bytes) => bytes,
            // A corrupt gzip stream is a malformed request, not an
            // unsupported one.
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        None => return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
    };
    let decoded = match format {
        WireFormat::Protobuf => decode_protobuf(&raw),
        WireFormat::Json => decode_json(&raw),
    };
    let Ok(request) = decoded else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // WAL-before-ack ingest. The lock spans only this synchronous call.
    // Recover the guard even if a prior holder panicked: a poisoned lock
    // must not turn into a panic here (the handler promises not to), so
    // take the inner guard regardless and let this request proceed.
    let outcome = pipeline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ingest(request);

    match outcome {
        Ok(_) => success_response(format),
        // A Resource that doesn't resolve to a tenant is a client error
        // (the whole batch is rejected, RFC0003.4).
        Err(ReceiveError::TenantResolution(_)) => StatusCode::BAD_REQUEST.into_response(),
        // A WAL failure is server-side; the batch was not acked (§3.4).
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Map `Content-Type` to a wire format, ignoring any `; charset=…`
/// parameters. `None` = missing or unsupported (→ 415).
fn content_type(headers: &HeaderMap) -> Option<WireFormat> {
    let value = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let media_type = value.split(';').next().unwrap_or_default().trim();
    match media_type {
        "application/x-protobuf" => Some(WireFormat::Protobuf),
        "application/json" => Some(WireFormat::Json),
        _ => None,
    }
}

/// Map `Content-Encoding` to a supported encoding. Absent or `identity`
/// is identity; `gzip` is supported; anything else (`zstd`, `br`, …) is
/// `None` (→ 415; zstd is deferred per §9).
fn content_encoding(headers: &HeaderMap) -> Option<Encoding> {
    match headers.get(header::CONTENT_ENCODING) {
        None => Some(Encoding::Identity),
        Some(value) => match value.to_str().ok()?.trim() {
            "" | "identity" => Some(Encoding::Identity),
            "gzip" => Some(Encoding::Gzip),
            _ => None,
        },
    }
}

fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

/// A 200 carrying an empty `ExportLogsServiceResponse` (`partial_success`
/// unset), encoded in the request's wire format.
fn success_response(format: WireFormat) -> Response {
    let response = ExportLogsServiceResponse::default();
    match format {
        WireFormat::Protobuf => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-protobuf")],
            response.encode_to_vec(),
        )
            .into_response(),
        WireFormat::Json => match serde_json::to_vec(&response) {
            Ok(body) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response(),
            // Encoding the (trivial) success response shouldn't fail; if
            // it ever did, a 500 is honest — never a 200 with an empty
            // body.
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
    }
}
