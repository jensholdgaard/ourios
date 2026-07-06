//! OTLP/HTTP listener (RFC 0003 §6.2).
//!
//! An `axum` `POST` handler at a configurable path (default `/v1/logs`)
//! that decodes the body per `Content-Type` (`application/x-protobuf` or
//! `application/json`) and `Content-Encoding` (`identity` or `gzip`),
//! hands the decoded `ExportLogsServiceRequest` to the `IngestPipeline`
//! (WAL-before-ack), and returns an `ExportLogsServiceResponse`.
//!
//! Transport errors are controlled (RFC0003.11): unsupported media type
//! / encoding → 415, malformed body → 400, oversize → 413, an
//! unconfigured path → 404, tenant-resolution failure → 400. No panics.
//!
//! The pipeline is shared behind a plain `Arc`: its group-commit
//! coordinator serializes the single-writer WAL internally (RFC 0008
//! §3.1) while letting concurrent requests batch their fsyncs
//! (RFC0008.8). `ingest` is async, so the handler simply `.await`s it.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceResponse;
use ourios_core::auth::TokenStore;
use prost::Message;

use crate::receiver::auth::authenticate_bearer;
use crate::receiver::decode::{decode_json, decode_protobuf};
use crate::receiver::pipeline::{ReceiveError, SharedPipeline};

/// OTLP/HTTP listener configuration.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The accepted `POST` path (default `/v1/logs`; configurable per
    /// §6.2 / RFC0003.14).
    pub path: String,
    /// Maximum request body size in bytes; a larger body is rejected with
    /// 413 (RFC0003.11).
    pub max_body_bytes: usize,
    /// The RFC 0026 token store; `None` (the default) is open mode
    /// (§3.1). With a store, every request must carry a known
    /// `Authorization: Bearer` credential (→ 401) and its batch is bound
    /// to the token's tenant set (→ 403).
    pub auth: Option<Arc<TokenStore>>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            path: "/v1/logs".to_owned(),
            max_body_bytes: 4 * 1024 * 1024,
            auth: None,
        }
    }
}

/// Handler state: the shared pipeline plus the decompressed-size cap
/// (`DefaultBodyLimit` only bounds the *compressed* body, so gzip is
/// bounded separately to defuse a decompression bomb) and the RFC 0026
/// token store.
#[derive(Clone)]
struct AppState {
    pipeline: SharedPipeline,
    max_decompressed_bytes: usize,
    auth: Option<Arc<TokenStore>>,
}

/// Build the OTLP/HTTP router over `pipeline`.
pub fn router(pipeline: SharedPipeline, config: &HttpConfig) -> Router {
    let state = AppState {
        pipeline,
        max_decompressed_bytes: config.max_body_bytes,
        auth: config.auth.clone(),
    };
    Router::new()
        .route(&config.path, post(handle_logs))
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        .with_state(state)
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

async fn handle_logs(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // RFC 0026 §3.2: authentication precedes everything — media-type
    // dispatch, decompression, and wire decode all happen only for an
    // authenticated (or open-mode) request.
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Ok(binding) = authenticate_bearer(state.auth.as_deref(), authorization) else {
        // RFC 0026 §3.4: the rejection counts on the request counter.
        state.pipeline.record_unauthenticated();
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let Some(format) = content_type(&headers) else {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    };
    let raw = match content_encoding(&headers) {
        Some(Encoding::Identity) => body.to_vec(),
        Some(Encoding::Gzip) => match gunzip(&body, state.max_decompressed_bytes) {
            Ok(bytes) => bytes,
            // Corrupt gzip is a malformed request (400); a body that
            // decompresses past the limit is too large (413) — a
            // decompression bomb, since DefaultBodyLimit only bounds the
            // compressed bytes.
            Err(GunzipError::Corrupt) => return StatusCode::BAD_REQUEST.into_response(),
            Err(GunzipError::TooLarge) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
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

    // WAL-before-ack ingest. The fsync is batched by the group-commit
    // coordinator (RFC0008.8), which offloads its blocking `sync`, so the
    // handler just awaits. Run it on its own task so a panic in the
    // pipeline/miner is contained as a 500 (the handler promises not to
    // panic) rather than aborting the connection.
    let pipeline = state.pipeline.clone();
    match tokio::spawn(async move { pipeline.ingest_bound(request, binding.as_ref()).await }).await
    {
        Ok(Ok(_)) => success_response(format),
        Ok(Err(e)) => ingest_error_status(&e).into_response(),
        // The ingest task panicked — a genuine, non-retryable internal bug.
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Map a settled ingest failure to its HTTP status (RFC 0018 §3.2).
///
/// Permanent client errors are non-retryable but split by class:
/// tenant-resolution failure → `400`; an oversize payload
/// (`AppendError::TooLarge`, over the 16 MiB WAL frame ceiling) → `413`. Any
/// other WAL append/sync failure is *transient* (the batch was not acked,
/// §3.4) → retryable `503`, so compliant clients re-send rather than drop
/// data (a non-retryable `500` would tell them to drop it).
///
/// Matched per-variant (exhaustive over `ReceiveError` in-crate): a future
/// `#[non_exhaustive]` variant breaks the build here, forcing a
/// retryable-vs-not decision rather than defaulting either way.
fn ingest_error_status(error: &ReceiveError) -> StatusCode {
    match error {
        ReceiveError::TenantResolution(_) => StatusCode::BAD_REQUEST,
        // An authenticated caller writing outside its tenant set — a
        // permanent authz rejection, whole batch, pre-WAL (RFC 0026 §3.2).
        ReceiveError::TenantDenied { .. } => StatusCode::FORBIDDEN,
        ReceiveError::WalAppend(ourios_wal::AppendError::TooLarge { .. }) => {
            StatusCode::PAYLOAD_TOO_LARGE
        }
        ReceiveError::WalAppend(_) | ReceiveError::WalSync(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Map `Content-Type` to a wire format, ignoring any `; charset=…`
/// parameters. `None` = missing or unsupported (→ 415).
fn content_type(headers: &HeaderMap) -> Option<WireFormat> {
    let value = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    // Media types are case-insensitive; ignore any `; charset=…` params.
    let media_type = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match media_type.as_str() {
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
        // Content-Encoding tokens are case-insensitive.
        Some(value) => match value.to_str().ok()?.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Some(Encoding::Identity),
            "gzip" => Some(Encoding::Gzip),
            _ => None,
        },
    }
}

/// Why a gzip body was rejected.
enum GunzipError {
    /// Not a valid gzip stream.
    Corrupt,
    /// The decompressed size exceeded `max` — a decompression bomb.
    TooLarge,
}

/// Decompress a gzip body, refusing to inflate past `max` bytes
/// (`DefaultBodyLimit` bounds only the compressed body, so an attacker
/// could otherwise expand a tiny upload into an unbounded allocation).
fn gunzip(bytes: &[u8], max: usize) -> Result<Vec<u8>, GunzipError> {
    use std::io::Read;
    // Read one byte past the cap so we can distinguish "exactly max" from
    // "over the cap".
    let cap = max.saturating_add(1) as u64;
    let mut decoder = flate2::read::GzDecoder::new(bytes).take(cap);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|_| GunzipError::Corrupt)?;
    if out.len() > max {
        return Err(GunzipError::TooLarge);
    }
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

#[cfg(test)]
mod tests {
    use super::{ReceiveError, StatusCode, ingest_error_status};
    use crate::receiver::tenant::TenantResolutionError;
    use ourios_wal::{AppendError, SyncError};

    #[test]
    fn tenant_resolution_is_400() {
        let e = ReceiveError::TenantResolution(TenantResolutionError::for_test("service.name"));
        assert_eq!(ingest_error_status(&e), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn tenant_denied_is_403() {
        let e = ReceiveError::TenantDenied {
            token_name: "edge".to_string(),
            tenant: ourios_core::tenant::TenantId::new("intruder"),
        };
        assert_eq!(ingest_error_status(&e), StatusCode::FORBIDDEN);
    }

    #[test]
    fn oversize_payload_is_413() {
        let e = ReceiveError::WalAppend(AppendError::TooLarge {
            len: 32 * 1024 * 1024,
            limit: 16 * 1024 * 1024,
        });
        assert_eq!(ingest_error_status(&e), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn transient_wal_append_is_503() {
        let e = ReceiveError::WalAppend(AppendError::Io {
            op: "write",
            source: std::io::Error::other("io"),
        });
        assert_eq!(ingest_error_status(&e), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn quiesced_wal_append_is_503() {
        let e = ReceiveError::WalAppend(AppendError::QuiescedAfterRotationFailure);
        assert_eq!(ingest_error_status(&e), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn transient_wal_sync_is_503() {
        let e = ReceiveError::WalSync(SyncError::Io {
            op: "fdatasync",
            source: std::io::Error::other("io"),
        });
        assert_eq!(ingest_error_status(&e), StatusCode::SERVICE_UNAVAILABLE);
    }
}
