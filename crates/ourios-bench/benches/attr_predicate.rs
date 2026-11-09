//! Attribute-predicate latency (RFC 0022 §3.3), the RFC0062.6 cost gate:
//! a literal dotted key versus a path into a map value (RFC 0062 §3.6).
//!
//! Every row carries a realistic `OTel` attribute set. Half the rows put
//! the user id under a map (`user = {id, tier}`), half under the literal
//! dotted key `user.id`, so `attr.user.id == "u7"` exercises both shapes.
//! Each query runs against a store written without promotion (the JSON
//! arm alone) and one with `http.route` and `user.id` promoted (the
//! two-arm form). The count each query returns is printed once, since
//! it is what differs between the literal and the path semantics.

use std::hint::black_box;
use std::path::Path;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use ourios_core::alias::AliasMap;
use ourios_core::audit::ParamType;
use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue, KeyValueList};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{DEFAULT_ZSTD_LEVEL, PartitionKey, PromotedAttributes, Store, Writer};
use ourios_querier::Querier;

const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;
const ROWS_PER_FILE: u64 = 20_000;
const FILES: u64 = 10;
const ROUTES: [&str; 20] = [
    "/cart",
    "/checkout",
    "/login",
    "/logout",
    "/search",
    "/product",
    "/product/reviews",
    "/account",
    "/account/orders",
    "/recommendations",
    "/ads",
    "/currency",
    "/shipping",
    "/payment",
    "/email",
    "/health",
    "/metrics",
    "/static",
    "/api/v1/items",
    "/api/v1/users",
];

const QUERIES: [(&str, &str); 5] = [
    ("literal-eq", r#"attr.http.route == "/cart" | count"#),
    ("literal-ne", r#"attr.http.route != "/cart" | count"#),
    ("literal-miss", r#"attr.http.route == "/nope" | count"#),
    ("path-eq", r#"attr.user.id == "u7" | count"#),
    ("path-ne", r#"attr.user.id != "u7" | count"#),
];

fn string(key: &str, value: impl Into<String>) -> KeyValue {
    any(key, Value::StringValue(value.into()))
}

fn any(key: &str, value: Value) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue { value: Some(value) }),
        ..Default::default()
    }
}

fn attributes(i: u64) -> Vec<KeyValue> {
    let user_id = format!("u{}", i % 1_000);
    #[allow(clippy::cast_possible_truncation)] // i % 20 < 20
    let route = ROUTES[(i % 20) as usize];
    let mut attrs = vec![
        string("http.route", route),
        string(
            "http.request.method",
            if i.is_multiple_of(3) { "POST" } else { "GET" },
        ),
        any(
            "http.response.status_code",
            Value::IntValue(if i.is_multiple_of(50) { 500 } else { 200 }),
        ),
        string("url.path", format!("{route}/{}", i % 97)),
        string("client.address", format!("10.0.{}.{}", i % 256, i % 251)),
        string(
            "user_agent.original",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko)",
        ),
    ];
    if (i / 1_000).is_multiple_of(2) {
        attrs.push(any(
            "user",
            Value::KvlistValue(KeyValueList {
                values: vec![string("id", user_id), string("tier", "gold")],
            }),
        ));
    } else {
        attrs.push(string("user.id", user_id));
        attrs.push(string("user.tier", "gold"));
    }
    attrs
}

fn rec(i: u64, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: 1 + i % 7,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.http".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
        attributes: attributes(i),
        dropped_attributes_count: 0,
        resource_attributes: vec![string("service.name", "frontend")],
        trace_id: None,
        span_id: None,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
        ingest_position: None,
    }
}

fn build(bucket: &Path, promoted: &PromotedAttributes) {
    let store = Store::local(bucket).expect("local store");
    for file in 0..FILES {
        let base = TS0 + file * HOUR_NS;
        let records: Vec<MinedRecord> = (0..ROWS_PER_FILE)
            .map(|i| rec(file * ROWS_PER_FILE + i, base + i * 1_000))
            .collect();
        let part = PartitionKey::derive(&records[0]).expect("derive partition");
        let mut w =
            Writer::open_in_with_promoted(&store, part, DEFAULT_ZSTD_LEVEL, promoted.clone())
                .expect("open writer");
        w.append_records(&records).expect("append");
        w.close().expect("close");
    }
}

fn attr_predicate(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");
    let tenant = TenantId::new("a");
    let now = TS0 + 24 * HOUR_NS;
    let window = 30 * 24 * HOUR_NS;
    let aliases = AliasMap::new();

    let mut group = c.benchmark_group("attr_predicate");
    for (store_id, promoted) in [
        ("json", PromotedAttributes::default()),
        (
            "promoted",
            PromotedAttributes::new([], ["http.route".to_string(), "user.id".to_string()]),
        ),
    ] {
        let bucket = tempfile::TempDir::new().expect("temp bucket");
        build(bucket.path(), &promoted);
        let querier = Querier::new(bucket.path());
        for (query_id, src) in QUERIES {
            let query = ourios_querier::dsl::parse(src).expect("parse");
            let run = || {
                rt.block_on(querier.run_query(&query, &tenant, now, window, Some(&aliases)))
                    .expect("query")
            };
            let probe = run();
            let matched: u64 = probe.aggregate.iter().flatten().map(|g| g.count).sum();
            eprintln!(
                "attr_predicate/{store_id}/{query_id}: {matched} rows; scanned {} row groups, {} B",
                probe.stats.row_groups_scanned, probe.stats.bytes_read,
            );
            group.bench_function(BenchmarkId::new(store_id, query_id), |b| {
                b.iter(|| black_box(run().rows));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, attr_predicate);
criterion_main!(benches);
