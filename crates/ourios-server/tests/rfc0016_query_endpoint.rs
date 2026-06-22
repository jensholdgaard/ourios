//! RFC 0016 — query-serving endpoint (HTTP query API over the logs DSL), the
//! §5 acceptance scenarios.
//!
//! `.1`–`.4` (the request/dispatch/error handler) drive the querier role's
//! `router` in-process via `tower::ServiceExt::oneshot` against a real RFC 0005
//! store. `.5`–`.7` (role env-gating, graceful shutdown, compose) need the
//! `main.rs` wiring and stay `#[ignore]`d until that slice; `.6`'s metric arm
//! lands with the observability slice.
//!
//! See `docs/rfcs/0016-query-serving-endpoint.md` §5 / §6.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{ParquetAuditSink, PartitionKey, Writer};
use ourios_server::querier::router;
use tower::ServiceExt;

/// A default window wide enough that the fixture rows (timestamped an hour
/// before the request's `now`) always fall inside the no-`range` look-back
/// `[now - W, now]`, with no dependence on the absolute wall clock.
const HUGE_WINDOW: u64 = 100 * 365 * 24 * 60 * 60 * 1_000_000_000;

/// One hour before the current wall clock, in unix nanos — a fixture timestamp
/// that is always in the recent past (so the look-back window covers it)
/// regardless of the machine's clock.
fn recent_ns() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::try_from(now.saturating_sub(60 * 60 * 1_000_000_000)).unwrap_or(0)
}

fn mined(tenant: &str, template_id: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: recent_ns(),
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ourios_core::audit::ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

fn write_records(bucket: &Path, recs: &[MinedRecord]) {
    let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
    for r in recs {
        by_part
            .entry(PartitionKey::derive(r).expect("derive partition"))
            .or_default()
            .push(r.clone());
    }
    for (part, rs) in by_part {
        let mut w = Writer::open(bucket, part).expect("open writer");
        w.append_records(&rs).expect("append");
        w.close().expect("close");
    }
}

/// A `template_widened` audit event for `tenant` / `template_id` at `ts_ns`,
/// so a drift query over the window finds a widening.
fn widened(tenant: &str, template_id: u64, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: UNIX_EPOCH + Duration::from_nanos(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Widened {
                old_version: 1,
                new_version: 2,
                old_template: "user <*>".to_string(),
                new_template: "user <*> <*>".to_string(),
                positions_widened: vec![2],
            },
        },
    }
}

/// `POST /v1/query` against `router`, optionally with an `X-Ourios-Tenant`
/// header. Returns the status + the parsed JSON body.
async fn post(
    bucket: &Path,
    tenant: Option<&str>,
    content_type: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(bucket.to_path_buf(), HUGE_WINDOW);
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/query")
        .header(header::CONTENT_TYPE, content_type);
    if let Some(t) = tenant {
        req = req.header("X-Ourios-Tenant", t);
    }
    let response = app
        .oneshot(
            req.body(Body::from(body.to_owned()))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Scenario RFC0016.1 — the querier role serves a DSL query end-to-end.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[tokio::test]
async fn rfc0016_1_querier_role_serves_a_dsl_query_end_to_end() {
    let bucket = tempfile::tempdir().unwrap();
    write_records(bucket.path(), &[mined("acme", 1)]);

    let (status, json) = post(
        bucket.path(),
        Some("acme"),
        "text/plain",
        "template_id == 1",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "served a 200: {json}");
    assert_eq!(json["rows"], 1, "the one matching row is counted");
    assert_eq!(
        json["records"].as_array().map(Vec::len),
        Some(1),
        "the matching row is returned",
    );
    // Pruning stats are present (the pillar-1 win is visible to the caller).
    assert!(json["stats"]["row_groups_scanned"].is_u64());
    assert!(json["stats"]["row_groups_pruned"].is_u64());
    assert!(json["stats"]["bytes_read"].is_u64());
    assert_eq!(json["records"][0]["template_id"], 1);
}

/// Scenario RFC0016.2 — tenant scoping is enforced at the API.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[tokio::test]
async fn rfc0016_2_tenant_scoping_is_enforced_at_the_api() {
    let bucket = tempfile::tempdir().unwrap();
    // Two tenants with *distinct* data: acme has template 1, other has
    // template 2. Querying `template_id == 2` as acme proves isolation — a
    // cross-tenant read would surface other's row; truncation can't explain a
    // zero result.
    write_records(bucket.path(), &[mined("acme", 1), mined("other", 2)]);

    // acme sees its own template-1 row.
    let (status, json) = post(
        bucket.path(),
        Some("acme"),
        "text/plain",
        "template_id == 1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["rows"], 1, "acme's own row is read");
    assert_eq!(json["records"][0]["template_id"], 1);

    // acme does NOT see other's template-2 row — never scanned.
    let (status, json) = post(
        bucket.path(),
        Some("acme"),
        "text/plain",
        "template_id == 2",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["rows"], 0, "another tenant's row is never read");

    // No tenant header → 400 from the server's header check.
    let (status, json) = post(bucket.path(), None, "text/plain", "template_id == 1").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing tenant header is 400"
    );
    assert_eq!(json["error"]["kind"], "missing_tenant");
}

/// Scenario RFC0016.1 (JSON request modes) — the `application/json` forms both
/// dispatch: the `{"query": …}` text wrapper and the structured-IR JSON
/// (RFC 0002 §6.4) each parse + execute to the same result as `text/plain`.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[tokio::test]
async fn rfc0016_1_json_request_modes_dispatch() {
    let bucket = tempfile::tempdir().unwrap();
    write_records(bucket.path(), &[mined("acme", 1)]);

    // `{"query": "<dsl text>"}` wrapper.
    let (status, json) = post(
        bucket.path(),
        Some("acme"),
        "application/json",
        r#"{"query": "template_id == 1"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "wrapper JSON served: {json}");
    assert_eq!(json["rows"], 1);

    // Structured-IR JSON (RFC 0002 §6.4): a match-all predicate, no stages —
    // matches the one stored row.
    let (status, json) = post(
        bucket.path(),
        Some("acme"),
        "application/json",
        r#"{"predicate":{"const":true},"stages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "structured-IR JSON served: {json}");
    assert_eq!(json["rows"], 1);
}

/// Scenario RFC0016.3 — a drift query routes to the drift path.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[tokio::test]
async fn rfc0016_3_a_drift_query_routes_to_the_drift_path() {
    let bucket = tempfile::tempdir().unwrap();
    // A widening at 2026-06-01T12:00:00Z (1_780_315_200 s), inside the query
    // window below.
    let mut sink = ParquetAuditSink::new(bucket.path());
    sink.emit(widened("acme", 5, 1_780_315_200_000_000_000));
    assert_eq!(sink.write_failures(), 0, "audit fixture persisted");

    let (status, json) = post(
        bucket.path(),
        Some("acme"),
        "text/plain",
        "drift from 2026-06-01T00:00:00Z to 2026-06-02T00:00:00Z",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "drift served a 200: {json}");
    let rows = json["rows"]
        .as_array()
        .expect("drift result has a rows array");
    assert_eq!(rows.len(), 1, "the one drifting template is returned");
    assert_eq!(rows[0]["template_id"], 5);
    assert_eq!(rows[0]["widening_count"], 1);
    assert_eq!(rows[0]["max_new_version"], 2);
}

/// Scenario RFC0016.4 — malformed DSL is a clean 400, no engine leak (H6).
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[tokio::test]
async fn rfc0016_4_malformed_dsl_is_a_clean_400_no_engine_leak() {
    let bucket = tempfile::tempdir().unwrap();

    for malformed in [
        "this is not a valid query",
        "template_id == ",
        "SELECT * FROM logs",
        "drift GROUP BY template_id",
    ] {
        let (status, json) = post(bucket.path(), Some("acme"), "text/plain", malformed).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{malformed:?} should be a 400, got {json}",
        );
        assert!(
            json["error"]["kind"].is_string() && json["error"]["message"].is_string(),
            "{malformed:?} returns an Ourios-owned error body: {json}",
        );
        // H6: no engine internals leak in the response.
        let shown = json.to_string().to_ascii_lowercase();
        for token in ["datafusion", "logicalplan", "physical_plan", "recordbatch"] {
            assert!(
                !shown.contains(token),
                "{malformed:?} leaked engine token {token:?}: {shown}",
            );
        }
    }
}

/// RFC0016.4 (resource bound) — an oversized request body is rejected with
/// `413` by the router's `DefaultBodyLimit`, not read into memory.
#[tokio::test]
async fn rfc0016_oversize_body_is_rejected() {
    let bucket = tempfile::tempdir().unwrap();
    let huge = "a".repeat(ourios_server::querier::MAX_BODY_BYTES + 1);
    let (status, _) = post(bucket.path(), Some("acme"), "text/plain", &huge).await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "an over-limit body is 413, never read whole into memory",
    );
}

// RFC0016.5 (role gating + graceful shutdown) and RFC0016.7 (receiver +
// querier compose) are process-level: they spawn the `ourios-server` binary
// and drive SIGTERM, so they live in the unix-gated `rfc0016_5_7_served_querier`
// integration test (mirroring the receiver's `rfc0003_16_served_binary`).

/// Scenario RFC0016.6 — pruning is observable.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[test]
#[ignore = "RFC0016.6 — red until pruning stats + OTel query metrics are emitted (green)"]
fn rfc0016_6_pruning_is_observable() {
    todo!("RFC0016.6: selective query → row_groups_pruned > 0 + latency/pruning-ratio metric")
}
