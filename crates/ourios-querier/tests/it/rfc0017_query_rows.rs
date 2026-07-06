//! RFC 0017 — read-time template registry & query-row rendering, the
//! typed-row-payload scenarios (`.6`, `.7`, `.8`).
//!
//! `.6` (the typed-row payload is returned, B1/B2-compatible), `.7` (no engine
//! internals leak — H6), and `.8` (every persisted OTLP field round-trips on
//! read) drive the row-returning `Querier::run` against a real RFC 0005 store.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.4 / §5 / §6.

use crate::common::{NOW, kv, rec, simple, write_all};

use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::canonical::encode_any_value;
use ourios_core::otlp::{AnyValue, KeyValueList};
use ourios_core::record::BodyKind;
use ourios_core::tenant::TenantId;
use ourios_querier::{LogBody, Querier, QueryRequest};
use tempfile::TempDir;

fn count_only() -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("acme"),
        time_range: None,
        template_id: None,
        severity_text: None,
        limit: None,
    }
}

/// Scenario RFC0017.6 — a query with a `limit` returns up to `limit` `LogRow`s
/// in `QueryResult.records`, while `rows` (the count) and `stats` are unchanged
/// so B1/B2 still hold.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[tokio::test]
async fn rfc0017_6_typed_row_payload_returned_b1b2_compatible() {
    let bucket = TempDir::new().unwrap();
    // Five matching rows under one tenant/template.
    let recs: Vec<_> = (0..5).map(|i| simple("acme", 1, NOW - 1 - i)).collect();
    write_all(bucket.path(), &recs);
    let querier = Querier::new(bucket.path());

    // Count-only (no limit): records empty, rows = 5.
    let counted = querier.run(count_only()).await.expect("count query");
    assert_eq!(counted.rows, 5, "count is the full matching total");
    assert!(
        counted.records.is_empty(),
        "no limit ⇒ count-only, records stays empty",
    );

    // With a limit: records capped at the limit; rows + stats unchanged.
    let limited = querier
        .run(QueryRequest {
            limit: Some(2),
            ..count_only()
        })
        .await
        .expect("limited query");
    assert_eq!(
        limited.rows, 5,
        "rows (the count) is unchanged by the limit"
    );
    assert_eq!(limited.records.len(), 2, "records is capped at the limit");
    assert_eq!(
        limited.stats, counted.stats,
        "the scan/pruning stats are unchanged (B1/B2)",
    );
}

/// Scenario RFC0017.7 — the returned `QueryResult` / `LogRow` / `LogBody`
/// surface carries no `arrow` / `DataFusion` / SQL type (hazard H6 / §4.6): a
/// regression that added an engine-typed field would surface its type name in
/// the value's `Debug`. Mirrors the RFC0007.3 / RFC0010.8 denylist technique.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[tokio::test]
async fn rfc0017_7_no_engine_internals_leak() {
    let bucket = TempDir::new().unwrap();
    write_all(bucket.path(), &[simple("acme", 1, NOW - 1)]);
    let result = Querier::new(bucket.path())
        .run(QueryRequest {
            limit: Some(10),
            ..count_only()
        })
        .await
        .expect("query");
    assert_eq!(result.records.len(), 1, "one row returned to inspect");

    let shown = format!("{result:?}").to_ascii_lowercase();
    for token in [
        "datafusion",
        "arrow",
        "recordbatch",
        "logicalplan",
        "physical_plan",
        "scalarvalue",
    ] {
        assert!(
            !shown.contains(token),
            "QueryResult/LogRow Debug leaked engine token {token:?}: {shown:?}",
        );
    }
}

/// Scenario RFC0017.8 — every persisted OTLP field round-trips on read: a stored
/// row carrying the full `LogRecord` field set comes back as a `LogRow` whose
/// every field equals what the schema stored, with `attributes` /
/// `resource_attributes` / `scope_attributes` decoded to structured key/values
/// (not opaque JSON), and no stored field dropped.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[tokio::test]
async fn rfc0017_8_every_persisted_otlp_field_round_trips() {
    let bucket = TempDir::new().unwrap();

    // A structured Body so the body round-trips without an audit-derived
    // registry (this fixture writes data only); every other OTLP field is set
    // to a distinctive value.
    let body_value = AnyValue {
        value: Some(Value::KvlistValue(KeyValueList {
            values: vec![kv("event", "login")],
        })),
    };
    let body_json = String::from_utf8(encode_any_value(&body_value).unwrap()).unwrap();

    // Start from the fully-populated fixture and override every field with a
    // distinctive value, so a dropped/garbled field fails the assertion.
    let mut stored = rec(
        "acme",
        7,
        NOW - 1,
        17,
        "checkout",
        "lib.cart",
        Some([7u8; 16]),
        Some([3u8; 8]),
    );
    stored.template_version = 2;
    stored.severity_text = Some("ERROR".to_string());
    stored.scope_version = Some("2.1.0".to_string());
    stored.scope_attributes = vec![kv("db.system", "postgres")];
    stored.resource_schema_url = Some("https://opentelemetry.io/schemas/1.31.0".to_string());
    stored.scope_schema_url = Some("https://opentelemetry.io/schemas/1.0.0".to_string());
    stored.observed_time_unix_nano = Some(NOW + 5);
    stored.attributes = vec![kv("http.method", "GET")];
    stored.dropped_attributes_count = 3;
    stored.resource_attributes = vec![kv("service.name", "checkout")];
    stored.flags = 0x01;
    stored.event_name = Some("login".to_string());
    stored.body_kind = BodyKind::Structured;
    stored.body = Some(body_json);

    write_all(bucket.path(), std::slice::from_ref(&stored));

    let result = Querier::new(bucket.path())
        .run(QueryRequest {
            limit: Some(10),
            ..count_only()
        })
        .await
        .expect("query");
    assert_eq!(result.records.len(), 1, "the one stored row is returned");
    let row = &result.records[0];

    assert_eq!(row.time_unix_nano, stored.time_unix_nano);
    assert_eq!(row.observed_time_unix_nano, stored.observed_time_unix_nano);
    assert_eq!(row.severity_number, stored.severity_number);
    assert_eq!(row.severity_text, stored.severity_text);
    assert_eq!(row.trace_id, stored.trace_id);
    assert_eq!(row.span_id, stored.span_id);
    assert_eq!(row.flags, stored.flags);
    assert_eq!(row.event_name, stored.event_name);
    assert_eq!(row.scope_name, stored.scope_name);
    assert_eq!(row.scope_version, stored.scope_version);
    // Attributes come back as structured key/values, not an opaque JSON blob.
    assert_eq!(row.scope_attributes, stored.scope_attributes);
    assert_eq!(row.attributes, stored.attributes);
    assert_eq!(row.resource_attributes, stored.resource_attributes);
    assert_eq!(row.resource_schema_url, stored.resource_schema_url);
    assert_eq!(row.scope_schema_url, stored.scope_schema_url);
    assert_eq!(
        row.dropped_attributes_count,
        stored.dropped_attributes_count
    );
    assert_eq!(row.template_id, stored.template_id);
    assert_eq!(row.template_version, stored.template_version);
    // The structured body round-trips as structure.
    assert_eq!(row.body, LogBody::Structured(body_value));
}
