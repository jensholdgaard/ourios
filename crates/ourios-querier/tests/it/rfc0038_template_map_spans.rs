//! RFC 0038 §3.7.2 — the template-map acquisition is a `load template_map`
//! span under the query's span, with the fold a `derive template_map` span
//! beneath it and a query waiting on another's acquisition a
//! `wait template_map` span.
//!
//! Each test drives its queries under a scoped subscriber over its own
//! tracer provider, so nothing global is installed. The fold opens its span
//! on the blocking pool; the acquisition hands the caller's subscriber to
//! that thread, which is what lets a scoped subscriber see it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use opentelemetry::trace::{SpanId, SpanKind, TraceContextExt as _, TracerProvider as _};
use opentelemetry::{KeyValue, Value};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use ourios_querier::Querier;
use ourios_semconv as semconv;
use tracing::Instrument as _;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

const TENANT: &str = "acme";
const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;
const NOW: u64 = TS0 + 24 * HOUR_NS;
const WINDOW_NS: u64 = 30 * 24 * HOUR_NS;

fn mined(ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(TENANT),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
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
    }
}

fn write(bucket: &Path, records: &[MinedRecord]) {
    let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
    for r in records {
        by_part
            .entry(PartitionKey::derive(r).expect("partition"))
            .or_default()
            .push(r.clone());
    }
    for (part, rs) in by_part {
        let mut w = Writer::open(bucket, part).expect("writer");
        w.append_records(&rs).expect("append");
        w.close().expect("close");
    }
}

/// One body-rendering query under a stand-in `POST /v1/query` span (the real
/// one is `ourios-server`'s); the stand-in's span id.
async fn query(querier: Arc<Querier>) -> SpanId {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let span = tracing::info_span!("POST /v1/query");
    let id = span.context().span().span_context().span_id();
    let parsed = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
    querier
        .run_query(&parsed, &TenantId::new(TENANT), NOW, WINDOW_NS, None)
        .instrument(span)
        .await
        .expect("query");
    id
}

fn named<'s>(spans: &'s [SpanData], name: &str) -> Vec<&'s SpanData> {
    spans.iter().filter(|s| s.name.as_ref() == name).collect()
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv: &KeyValue| kv.value.clone())
}

fn text(span: &SpanData, key: &str) -> Option<String> {
    attribute(span, key).map(|v| v.as_str().into_owned())
}

/// A scoped tracing pipeline over its own in-memory span exporter. Keep the
/// provider alive until the spans are read.
fn traced() -> (InMemorySpanExporter, SdkTracerProvider, tracing::Dispatch) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test"))),
    );
    (exporter, provider, dispatch)
}

/// A cold acquisition: `load template_map` under the query, its `miss`
/// outcome recorded, and the fold a `derive template_map` child of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_acquisition_spans_the_load_and_its_derive_under_the_query() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), &[mined(TS0)]);
    let querier = Arc::new(Querier::new(dir.path()));
    let query_id = query(querier).with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [load] = named(&spans, "load template_map")[..] else {
        panic!(
            "one load span: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert_eq!(load.span_kind, SpanKind::Internal);
    assert_eq!(load.parent_span_id, query_id, "load nests under the query");
    assert_eq!(
        text(load, semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME).as_deref(),
        Some("miss")
    );
    assert_eq!(text(load, semconv::OURIOS_TENANT).as_deref(), Some(TENANT));

    let [derive] = named(&spans, "derive template_map")[..] else {
        panic!("one derive span");
    };
    assert_eq!(derive.span_kind, SpanKind::Internal);
    assert_eq!(
        derive.parent_span_id,
        load.span_context.span_id(),
        "the fold nests under the acquisition"
    );
    assert_eq!(
        text(derive, semconv::OURIOS_TEMPLATE_MAP_DERIVE_REASON).as_deref(),
        Some("miss")
    );
    assert!(derive.start_time >= load.start_time && derive.end_time <= load.end_time);
}

/// Two concurrent queries at one frontier: the one that waits on the
/// other's acquisition shows a `wait template_map` span under its own
/// `load template_map`, whose outcome is `joined`.
#[cfg(feature = "testing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_query_joining_an_acquisition_spans_its_wait() {
    use std::time::{Duration, Instant};

    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), &[mined(TS0)]);
    let querier = Arc::new(Querier::new(dir.path()));
    let weak = Arc::downgrade(&querier);
    querier.set_template_map_acquire_gate(Some(Arc::new(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(querier) = weak.upgrade()
            && querier.template_map_waits() < 1
        {
            assert!(Instant::now() < deadline, "the second query never waited");
            std::thread::sleep(Duration::from_millis(2));
        }
    })));
    let tasks: Vec<_> = (0..2)
        .map(|_| tokio::spawn(query(Arc::clone(&querier)).with_subscriber(dispatch.clone())))
        .collect();
    for task in tasks {
        task.await.expect("query task");
    }
    querier.set_template_map_acquire_gate(None);

    let spans = exporter.get_finished_spans().expect("spans");
    let [wait] = named(&spans, "wait template_map")[..] else {
        panic!("one wait span");
    };
    let load = named(&spans, "load template_map")
        .into_iter()
        .find(|s| s.span_context.span_id() == wait.parent_span_id)
        .expect("the wait nests under a load span");
    assert_eq!(
        text(load, semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME).as_deref(),
        Some("joined")
    );
}
