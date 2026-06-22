//! The querier role (RFC 0016): an HTTP/JSON query API over the logs DSL
//! (RFC 0002), executed by the `ourios-querier` engine (RFC 0007).
//!
//! `POST /v1/query` accepts a DSL statement — `text/plain` (the text grammar)
//! or `application/json` (either a `{"query": "<dsl>"}` wrapper or RFC 0002's
//! structured-IR JSON) — with a required `X-Ourios-Tenant` header. A `Logs`
//! statement runs through [`Querier::run_query`], a `Drift` statement through
//! [`Querier::run_drift`] (RFC 0010). The reply is `200` JSON: the matching
//! rows (rendered [`LogRow`]s, RFC 0017) plus the pruning stats
//! (`row_groups_scanned` / `row_groups_pruned` / `bytes_read`) so a caller sees
//! the pillar-1 win directly.
//!
//! **H6.** Every error is Ourios-owned; no `DataFusion` type, SQL string, or
//! query plan ever appears in a response — DSL/compile failures map to `400`
//! with `{ "error": { "kind", "message" } }`, and an execution failure to
//! `500` whose message is the engine's already-scrubbed `Display` (RFC0007.3).
//!
//! `serve` / `QuerierHandle` mirror the receiver role's topology (RFC 0003):
//! bind a listener, serve it with `axum::serve(...).with_graceful_shutdown`,
//! and expose the bound address + a `shutdown()` future over a `watch` channel.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use ourios_core::otlp::canonical;
use ourios_core::tenant::TenantId;
use ourios_miner::reconstruct::Reconstruction;
use ourios_querier::dsl::ir::Stage;
use ourios_querier::dsl::{self, Statement};
use ourios_querier::{DriftResult, LogBody, LogRow, Querier, QueryResult, QueryStats};

/// The `X-Ourios-Tenant` request header (RFC 0016 §3.3) — kept out of the DSL
/// body so the grammar stays tenant-agnostic.
const TENANT_HEADER: &str = "x-ourios-tenant";

/// Default rows returned when a query carries no `limit` stage (RFC 0016 §7).
/// The endpoint returns rows by design, so an unbounded query is given this
/// cap rather than a count-only result (RFC 0017 populates `records` only when
/// the query has a limit).
pub const DEFAULT_LIMIT: u64 = 1000;
/// Hard cap on returned rows (RFC 0016 §7): a query's own `limit` is clamped to
/// this so a client can't request an unbounded materialisation.
pub const MAX_LIMIT: u64 = 10_000;
/// Max request-body size (a DSL statement is small; cap it so an oversized body
/// can't exhaust memory via the `Bytes` extractor — mirrors the receiver's
/// `DefaultBodyLimit`).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Querier role configuration (RFC 0016 §3.2).
#[derive(Debug, Clone)]
pub struct QuerierConfig {
    /// The HTTP listen address (`OURIOS_QUERIER_HTTP_ADDR`).
    pub http_addr: SocketAddr,
    /// Root of the RFC 0005 store to query (`OURIOS_BUCKET_ROOT`).
    pub bucket_root: PathBuf,
    /// The look-back applied to a query with no `range(...)` stage — the
    /// server-supplied default window the DSL compiler expects (RFC 0002 §4 P5;
    /// RFC 0016 §7).
    pub default_window_nanos: u64,
}

/// A running querier role: the resolved bound address plus the handle to shut
/// it down (mirrors `ReceiverHandle`).
pub struct QuerierHandle {
    pub http_addr: SocketAddr,
    shutdown: watch::Sender<()>,
    http: JoinHandle<std::io::Result<()>>,
}

impl QuerierHandle {
    /// Signal the listener to stop and await its graceful drain. A send error
    /// just means the listener already stopped.
    ///
    /// # Errors
    ///
    /// The listener task's join/serve error, as a `String` (no engine type
    /// crosses the boundary).
    pub async fn shutdown(self) -> Result<(), String> {
        let _ = self.shutdown.send(());
        self.http
            .await
            .map_err(|e| format!("HTTP listener task: {e}"))?
            .map_err(|e| format!("HTTP listener: {e}"))
    }
}

/// Shared handler state: the engine + the default window.
#[derive(Clone)]
struct QuerierState {
    querier: Arc<Querier>,
    default_window_nanos: u64,
}

/// Build the querier role's axum router over `state` (RFC 0016 §3.3). Split out
/// from [`serve`] so it can be driven in-process by tests.
pub fn router(bucket_root: PathBuf, default_window_nanos: u64) -> Router {
    let state = QuerierState {
        querier: Arc::new(Querier::new(bucket_root)),
        default_window_nanos,
    };
    Router::new()
        .route("/v1/query", post(handle_query))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Serve the querier role per `config` (RFC 0016 §3.2). Binds the listener
/// (so a `:0` request resolves to a real port in the returned handle), then
/// serves with graceful shutdown over a `watch` channel.
///
/// # Errors
///
/// A `String` describing a store-root creation, bind, or `local_addr` failure.
pub async fn serve(config: QuerierConfig) -> Result<QuerierHandle, String> {
    // The store root may not exist yet (a fresh dir); the querier only reads,
    // but binding before the dir exists would make the first query fail with a
    // confusing not-found rather than an empty result — create it, matching the
    // receiver role.
    std::fs::create_dir_all(&config.bucket_root)
        .map_err(|e| format!("create store root {}: {e}", config.bucket_root.display()))?;

    let listener = TcpListener::bind(config.http_addr)
        .await
        .map_err(|e| format!("bind HTTP {}: {e}", config.http_addr))?;
    let http_addr = listener
        .local_addr()
        .map_err(|e| format!("HTTP local_addr: {e}"))?;

    let app = router(config.bucket_root, config.default_window_nanos);
    let (shutdown, mut shutdown_rx) = watch::channel(());
    let http = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
    });

    Ok(QuerierHandle {
        http_addr,
        shutdown,
        http,
    })
}

/// `POST /v1/query` handler (RFC 0016 §3.3–§3.5).
async fn handle_query(
    State(state): State<QuerierState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Tenant is required and checked here, before the engine is invoked
    // (RFC 0016 §3.3): a missing/empty header is a `400` that never scans data.
    let Some(tenant) = tenant_from_headers(&headers) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_tenant",
            "the X-Ourios-Tenant header is required and must be non-empty",
        );
    };

    let statement = match parse_body(&headers, &body) {
        Ok(statement) => statement,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, "invalid_query", &message),
    };

    let now = now_unix_nano();
    match statement {
        Statement::Logs(mut query) => {
            apply_limit(&mut query.stages, DEFAULT_LIMIT, MAX_LIMIT);
            match state
                .querier
                .run_query(&query, &tenant, now, state.default_window_nanos, None)
                .await
            {
                Ok(result) => json_ok(&LogQueryResponse::from(&result)),
                Err(e) => query_error_response(&e),
            }
        }
        Statement::Drift(query) => match state.querier.run_drift(&query, &tenant, now).await {
            Ok(result) => json_ok(&DriftResponse::from(&result)),
            Err(e) => query_error_response(&e),
        },
    }
}

/// Read + validate the `X-Ourios-Tenant` header. `None` when absent, non-UTF-8,
/// or empty (all → `400` at the call site).
fn tenant_from_headers(headers: &HeaderMap) -> Option<TenantId> {
    let value = headers.get(TENANT_HEADER)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| TenantId::new(value))
}

/// Parse the body into a [`Statement`] by `Content-Type` (RFC 0016 §3.3):
/// `text/plain` → the text grammar; `application/json` → a `{"query": "<dsl>"}`
/// wrapper (unwrapped, then the text grammar) or the structured-IR JSON.
/// Returns the DSL error's message on failure.
fn parse_body(headers: &HeaderMap, body: &[u8]) -> Result<Statement, String> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();

    let text =
        || std::str::from_utf8(body).map_err(|_| "request body is not valid UTF-8".to_owned());

    if content_type == "application/json" {
        let json = text()?;
        // Distinguish the `{"query": "<dsl text>"}` wrapper from the
        // structured-IR JSON by whether the body is that wrapper object.
        if let Ok(QueryWrapper { query }) = serde_json::from_str::<QueryWrapper>(json) {
            dsl::parse_statement(&query).map_err(|e| e.to_string())
        } else {
            dsl::parse_structured_statement(json).map_err(|e| e.to_string())
        }
    } else {
        // `text/plain` (and the unset / any-other default): the raw statement.
        dsl::parse_statement(text()?).map_err(|e| e.to_string())
    }
}

/// Ensure the query's stages carry a `limit` no larger than `cap` (RFC 0016
/// §7): a query's own limit is clamped to `cap`; a query with none gets
/// `default`. Normalizes to **exactly one** `Limit` stage so the engine returns
/// rows.
fn apply_limit(stages: &mut Vec<Stage>, default: u64, cap: u64) {
    // The compiler reads the *last* `Limit`. Normalize to exactly one — the
    // clamped last — keeping its position relative to the non-limit stages
    // (stage order is DSL pipeline semantics, RFC 0002) and dropping any
    // earlier shadowed limits. A query with no limit gets `default`, itself
    // clamped to `cap` so the "no larger than cap" contract holds even if a
    // caller misconfigures `default > cap`.
    let Some(last) = stages.iter().rposition(|s| matches!(s, Stage::Limit(_))) else {
        stages.push(Stage::Limit(default.min(cap)));
        return;
    };
    let Stage::Limit(value) = stages[last] else {
        unreachable!("rposition matched a Limit stage")
    };
    let clamped = value.min(cap);
    let mut idx = 0;
    stages.retain(|s| {
        let keep = !matches!(s, Stage::Limit(_)) || idx == last;
        idx += 1;
        keep
    });
    if let Some(Stage::Limit(n)) = stages.iter_mut().find(|s| matches!(s, Stage::Limit(_))) {
        *n = clamped;
    }
}

fn now_unix_nano() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

// ---- response shapes (RFC 0016 §3.4) — all Ourios-owned, no engine type ----

#[derive(Serialize)]
struct StatsDto {
    row_groups_scanned: u64,
    row_groups_pruned: u64,
    bytes_read: u64,
}

impl From<&QueryStats> for StatsDto {
    fn from(s: &QueryStats) -> Self {
        Self {
            row_groups_scanned: s.row_groups_scanned,
            row_groups_pruned: s.row_groups_pruned,
            bytes_read: s.bytes_read,
        }
    }
}

#[derive(Serialize)]
struct LogQueryResponse {
    /// Total matching rows (the count) — unbounded by the `limit` (RFC 0017).
    rows: u64,
    stats: StatsDto,
    /// The returned rows, ≤ the effective limit.
    records: Vec<LogRowDto>,
}

impl From<&QueryResult> for LogQueryResponse {
    fn from(r: &QueryResult) -> Self {
        Self {
            rows: r.rows,
            stats: StatsDto::from(&r.stats),
            records: r.records.iter().map(LogRowDto::from).collect(),
        }
    }
}

#[derive(Serialize)]
struct LogRowDto {
    time_unix_nano: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_time_unix_nano: Option<u64>,
    severity_number: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    severity_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span_id: Option<String>,
    flags: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_version: Option<String>,
    scope_attributes: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_schema_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_schema_url: Option<String>,
    attributes: serde_json::Value,
    resource_attributes: serde_json::Value,
    dropped_attributes_count: u32,
    template_id: u64,
    template_version: u32,
    body: LogBodyDto,
}

impl From<&LogRow> for LogRowDto {
    fn from(row: &LogRow) -> Self {
        Self {
            time_unix_nano: row.time_unix_nano,
            observed_time_unix_nano: row.observed_time_unix_nano,
            severity_number: row.severity_number,
            severity_text: row.severity_text.clone(),
            trace_id: row.trace_id.as_ref().map(|b| hex(b)),
            span_id: row.span_id.as_ref().map(|b| hex(b)),
            flags: row.flags,
            event_name: row.event_name.clone(),
            scope_name: row.scope_name.clone(),
            scope_version: row.scope_version.clone(),
            scope_attributes: attributes_json(&row.scope_attributes),
            resource_schema_url: row.resource_schema_url.clone(),
            scope_schema_url: row.scope_schema_url.clone(),
            attributes: attributes_json(&row.attributes),
            resource_attributes: attributes_json(&row.resource_attributes),
            dropped_attributes_count: row.dropped_attributes_count,
            template_id: row.template_id,
            template_version: row.template_version,
            body: LogBodyDto::from(&row.body),
        }
    }
}

/// The rendered body (RFC 0017 §3.4): a string body as the reconstructed line +
/// its marker, a structured body as the typed `AnyValue` (proto3-JSON).
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LogBodyDto {
    Rendered {
        /// The rendered line as text (UTF-8, lossy for non-UTF-8 bytes).
        line: String,
        reconstruction: &'static str,
    },
    Structured {
        value: serde_json::Value,
    },
}

impl From<&LogBody> for LogBodyDto {
    fn from(body: &LogBody) -> Self {
        match body {
            LogBody::Rendered {
                line,
                reconstruction,
            } => Self::Rendered {
                line: String::from_utf8_lossy(line).into_owned(),
                reconstruction: match reconstruction {
                    Reconstruction::Faithful => "faithful",
                    // `Reconstruction` is `#[non_exhaustive]`; `RetainedVerbatim`
                    // and any future non-faithful marker serialise as
                    // `"retained_verbatim"`.
                    _ => "retained_verbatim",
                },
            },
            LogBody::Structured(value) => Self::Structured {
                value: any_value_json(value),
            },
            // `LogBody` is `#[non_exhaustive]`; a future body shape degrades to
            // an empty retained line rather than failing the whole response.
            _ => Self::Rendered {
                line: String::new(),
                reconstruction: "retained_verbatim",
            },
        }
    }
}

#[derive(Serialize)]
struct DriftResponse {
    rows: Vec<DriftRowDto>,
    stats: StatsDto,
}

#[derive(Serialize)]
struct DriftRowDto {
    template_id: u64,
    widening_count: u64,
    min_old_version: u32,
    max_new_version: u32,
    first_seen_unix_nano: u64,
    last_seen_unix_nano: u64,
}

impl From<&DriftResult> for DriftResponse {
    fn from(r: &DriftResult) -> Self {
        Self {
            rows: r
                .rows
                .iter()
                .map(|row| DriftRowDto {
                    template_id: row.template_id,
                    widening_count: row.widening_count,
                    min_old_version: row.min_old_version,
                    max_new_version: row.max_new_version,
                    first_seen_unix_nano: system_time_nanos(row.first_seen),
                    last_seen_unix_nano: system_time_nanos(row.last_seen),
                })
                .collect(),
            stats: StatsDto::from(&r.stats),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    kind: &'static str,
    message: String,
}

// ---- helpers ----

/// Decode an attribute list to its proto3-JSON value via the RFC 0005 §3.3
/// canonical encoder (the same encoding stored on disk), so the response never
/// carries an opaque JSON blob and needs no `opentelemetry-proto` serde feature.
fn attributes_json(attrs: &[ourios_core::otlp::KeyValue]) -> serde_json::Value {
    canonical::encode_attributes(attrs)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()))
}

fn any_value_json(value: &ourios_core::otlp::AnyValue) -> serde_json::Value {
    canonical::encode_any_value(value)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn system_time_nanos(t: SystemTime) -> u64 {
    let nanos = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

/// Lowercase-hex encode a byte slice (proto3-JSON convention for trace/span
/// ids). No `hex` crate dependency for a handful of fixed-width ids.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `200` with a JSON body. A serialisation failure (should not happen for these
/// owned DTOs) degrades to a `500` rather than a `200` with an empty body.
fn json_ok<T: Serialize>(value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "failed to encode the query result",
        ),
    }
}

/// A `status` JSON error body `{ "error": { "kind", "message" } }` (RFC 0016
/// §3.5). `message` is Ourios-owned text — never engine internals.
fn error_response(status: StatusCode, kind: &'static str, message: &str) -> Response {
    let body = ErrorBody {
        error: ErrorDetail {
            kind,
            message: message.to_owned(),
        },
    };
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{\"error\":{}}".to_vec());
    (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
}

/// Map a [`QueryError`] to its HTTP response (H6): a compile/validation failure
/// is a `400`; an execution/storage failure is a `500` whose message is the
/// engine's already-scrubbed `Display` (no DataFusion/SQL text — RFC0007.3).
fn query_error_response(error: &ourios_querier::QueryError) -> Response {
    use ourios_querier::QueryError;
    match error {
        QueryError::InvalidQuery { detail } => {
            error_response(StatusCode::BAD_REQUEST, "invalid_query", detail)
        }
        // The server's header check makes `TenantRequired` unreachable, but map
        // it defensively to the same `400` rather than leak it as a 500.
        QueryError::TenantRequired => error_response(
            StatusCode::BAD_REQUEST,
            "missing_tenant",
            "the X-Ourios-Tenant header is required and must be non-empty",
        ),
        // `Storage` and any future `#[non_exhaustive]` variant → a scrubbed
        // 500. `QueryError::Display` is the H6-safe surface (it withholds the
        // engine detail), so this never leaks DataFusion/SQL text.
        _ => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &error.to_string(),
        ),
    }
}

/// The `{"query": "<dsl text>"}` JSON wrapper (RFC 0016 §3.3). `deny_unknown_fields`
/// keeps the wrapper unambiguous against the structured-IR JSON: a structured
/// statement object (which carries `predicate` / `stages`, not a lone `query`
/// string) fails to parse as the wrapper and falls through to the structured
/// parser, rather than being mis-read as the wrapper.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryWrapper {
    query: String,
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::{
        DEFAULT_LIMIT, MAX_LIMIT, Stage, Statement, apply_limit, parse_body, tenant_from_headers,
    };

    fn headers(content_type: Option<&str>, tenant: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(ct) = content_type {
            h.insert(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap());
        }
        if let Some(t) = tenant {
            h.insert("x-ourios-tenant", HeaderValue::from_str(t).unwrap());
        }
        h
    }

    #[test]
    fn tenant_header_present_absent_empty() {
        assert_eq!(
            tenant_from_headers(&headers(None, Some("acme")))
                .as_ref()
                .map(ourios_core::tenant::TenantId::as_str),
            Some("acme"),
        );
        assert!(tenant_from_headers(&headers(None, None)).is_none());
        assert!(tenant_from_headers(&headers(None, Some("   "))).is_none());
    }

    #[test]
    fn apply_limit_defaults_clamps_and_keeps_one() {
        // No limit → DEFAULT injected.
        let mut stages = vec![];
        apply_limit(&mut stages, DEFAULT_LIMIT, MAX_LIMIT);
        assert_eq!(stages, vec![Stage::Limit(DEFAULT_LIMIT)]);

        // Existing within cap → kept; exactly one Limit remains.
        let mut stages = vec![Stage::Limit(5)];
        apply_limit(&mut stages, DEFAULT_LIMIT, MAX_LIMIT);
        assert_eq!(stages, vec![Stage::Limit(5)]);

        // Over cap → clamped.
        let mut stages = vec![Stage::Limit(MAX_LIMIT + 1)];
        apply_limit(&mut stages, DEFAULT_LIMIT, MAX_LIMIT);
        assert_eq!(stages, vec![Stage::Limit(MAX_LIMIT)]);

        // Multiple limits → normalized to one (the clamped last), at the last
        // one's position, earlier limits dropped, non-limit stages untouched.
        let sort = Stage::Sort {
            key: "count".to_owned(),
            desc: true,
        };
        let mut stages = vec![sort.clone(), Stage::Limit(5), Stage::Limit(MAX_LIMIT + 100)];
        apply_limit(&mut stages, DEFAULT_LIMIT, MAX_LIMIT);
        assert_eq!(
            stages,
            vec![sort, Stage::Limit(MAX_LIMIT)],
            "exactly one limit (the clamped last) remains after the non-limit stage",
        );
    }

    #[test]
    fn parse_body_text_plain_and_json_modes() {
        // text/plain → the raw grammar.
        assert!(matches!(
            parse_body(&headers(Some("text/plain"), None), b"template_id == 1"),
            Ok(Statement::Logs(_)),
        ));
        // application/json `{"query": …}` wrapper → the text grammar.
        assert!(matches!(
            parse_body(
                &headers(Some("application/json"), None),
                br#"{"query": "template_id == 1"}"#,
            ),
            Ok(Statement::Logs(_)),
        ));
        // application/json structured-IR (RFC 0002 §6.4) → the structured
        // parser. A match-all predicate with no stages is the minimal valid
        // structured log query.
        assert!(matches!(
            parse_body(
                &headers(Some("application/json"), None),
                br#"{"predicate":{"const":true},"stages":[]}"#,
            ),
            Ok(Statement::Logs(_)),
        ));
        // A drift statement still routes through the grammar.
        assert!(matches!(
            parse_body(
                &headers(Some("text/plain"), None),
                b"drift from 2026-06-01T00:00:00Z to 2026-06-02T00:00:00Z",
            ),
            Ok(Statement::Drift(_)),
        ));
        // Malformed → Err (mapped to 400 by the caller).
        assert!(parse_body(&headers(Some("text/plain"), None), b"not a query").is_err());
    }
}
