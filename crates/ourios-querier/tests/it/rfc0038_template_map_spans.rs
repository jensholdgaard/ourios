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
    let (id, result) = query_outcome(querier).await;
    result.expect("query");
    id
}

/// [`query`], returning the query's result instead of expecting success.
async fn query_outcome(
    querier: Arc<Querier>,
) -> (
    SpanId,
    Result<ourios_querier::QueryResult, ourios_querier::QueryError>,
) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let span = tracing::info_span!("POST /v1/query");
    let id = span.context().span().span_context().span_id();
    let parsed = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
    let result = querier
        .run_query(&parsed, &TenantId::new(TENANT), NOW, WINDOW_NS, None)
        .instrument(span)
        .await;
    (id, result)
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
    crate::common::global_subscriber();
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let dispatch =
        tracing::Dispatch::new(tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(
                provider.tracer_with_scope(ourios_core::telemetry::scope("ourios-test")),
            ),
        ));
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
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), &[mined(TS0)]);
    let querier = Arc::new(Querier::new(dir.path()));
    gate_until_one_waits(&querier);
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
    let load_id = load.span_context.span_id();
    assert!(
        named(&spans, "derive template_map")
            .iter()
            .all(|d| d.parent_span_id != load_id),
        "the joined query folds nothing of its own"
    );
}

/// One committed audit file for the tenant, then overwritten with bytes no
/// Parquet reader accepts, so the fold over it fails.
fn unreadable_audit_file(root: &Path) {
    audit_file(root, 1_775_127_480);
    let mut dirs = vec![root.join("audit")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                std::fs::write(&path, b"not parquet").expect("corrupt the file");
            }
        }
    }
}

/// One committed audit file for the tenant, at `secs` past the epoch.
fn audit_file(root: &Path, secs: u64) {
    use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink as _, TemplateChange};

    let store = ourios_parquet::Store::local(root).expect("store");
    let mut sink = ourios_parquet::ParquetAuditSink::new(store);
    sink.emit(AuditEvent {
        tenant_id: TenantId::new(TENANT),
        timestamp: std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        payload: AuditPayload::Template {
            template_id: 1,
            triggering_line_hash: ourios_core::audit::hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: "user <*>".to_owned(),
            },
        },
    });
    assert_eq!(sink.write_failures(), 0, "the fixture event persists");
}

/// One query's `load template_map` outcome and the `derive.reason` of its
/// fold, if it folded.
async fn lookup(
    querier: &Arc<Querier>,
    exporter: &InMemorySpanExporter,
    dispatch: &tracing::Dispatch,
) -> (Option<String>, Option<String>) {
    exporter.reset();
    query(Arc::clone(querier))
        .with_subscriber(dispatch.clone())
        .await;
    let spans = exporter.get_finished_spans().expect("spans");
    let [load] = named(&spans, "load template_map")[..] else {
        panic!("one load span");
    };
    let derive = named(&spans, "derive template_map");
    assert!(derive.len() <= 1, "at most one fold: {derive:?}");
    if let Some(derive) = derive.first() {
        assert_eq!(derive.parent_span_id, load.span_context.span_id());
    }
    (
        text(load, semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME),
        derive
            .first()
            .and_then(|d| text(d, semconv::OURIOS_TEMPLATE_MAP_DERIVE_REASON)),
    )
}

/// RFC0038.1's derive rule over a tenant's life: a cold lookup folds
/// (`miss`), the next one hits the published map and opens no fold, and
/// after a new audit file the stale map is refolded (`stale`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_lookup_that_folds_opens_a_derive_span() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), &[mined(TS0)]);
    audit_file(dir.path(), 1_775_127_480);
    let querier = Arc::new(Querier::new(dir.path()));
    let miss = (Some("miss".to_owned()), Some("miss".to_owned()));
    assert_eq!(lookup(&querier, &exporter, &dispatch).await, miss);
    let hit = (Some("hit".to_owned()), None);
    assert_eq!(lookup(&querier, &exporter, &dispatch).await, hit);
    audit_file(dir.path(), 1_775_131_080);
    let stale = (Some("stale".to_owned()), Some("stale".to_owned()));
    assert_eq!(lookup(&querier, &exporter, &dispatch).await, stale);
}

/// A fold that fails marks both `derive template_map` and `load
/// template_map` `Error` with the error class, and the query fails with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_fold_marks_the_derive_and_load_spans_error() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), &[mined(TS0)]);
    unreadable_audit_file(dir.path());
    let querier = Arc::new(Querier::new(dir.path()));
    let (_, result) = query_outcome(querier).with_subscriber(dispatch).await;
    assert!(result.is_err(), "an unreadable audit file fails the query");

    let spans = exporter.get_finished_spans().expect("spans");
    for name in ["derive template_map", "load template_map"] {
        let [span] = named(&spans, name)[..] else {
            panic!("one {name} span");
        };
        assert!(
            matches!(span.status, opentelemetry::trace::Status::Error { .. }),
            "{name}: {:?}",
            span.status
        );
        assert_eq!(
            text(span, "error.type").as_deref(),
            Some("storage"),
            "{name}"
        );
    }
    let [load] = named(&spans, "load template_map")[..] else {
        panic!("one load span");
    };
    assert_eq!(
        text(load, semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME).as_deref(),
        Some("miss"),
        "the failed lookup still says why it folded"
    );
}

/// Hold each acquisition until another query waits on it, so two
/// concurrent queries at one frontier share it.
#[cfg(feature = "testing")]
fn gate_until_one_waits(querier: &Arc<Querier>) {
    use std::time::{Duration, Instant};

    let weak = Arc::downgrade(querier);
    querier.set_template_map_acquire_gate(Some(Arc::new(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(querier) = weak.upgrade()
            && querier.template_map_waits() < 1
        {
            assert!(Instant::now() < deadline, "the second query never waited");
            std::thread::sleep(Duration::from_millis(2));
        }
    })));
}

/// A shared acquisition at one frontier that fails: the waiter's
/// `wait template_map` stays `Unset` (the wait itself did not fail), and its
/// `load template_map` is `Error` with the shared failure's class.
#[cfg(feature = "testing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_shared_acquisition_fails_the_waiters_load_not_its_wait() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), &[mined(TS0)]);
    unreadable_audit_file(dir.path());
    let querier = Arc::new(Querier::new(dir.path()));
    gate_until_one_waits(&querier);
    let tasks: Vec<_> = (0..2)
        .map(|_| {
            tokio::spawn(query_outcome(Arc::clone(&querier)).with_subscriber(dispatch.clone()))
        })
        .collect();
    for task in tasks {
        let (_, result) = task.await.expect("query task");
        assert!(
            result.is_err(),
            "a failed shared acquisition fails both queries"
        );
    }
    querier.set_template_map_acquire_gate(None);

    let spans = exporter.get_finished_spans().expect("spans");
    let [wait] = named(&spans, "wait template_map")[..] else {
        panic!("one wait span");
    };
    assert_eq!(wait.status, opentelemetry::trace::Status::Unset);
    let load = named(&spans, "load template_map")
        .into_iter()
        .find(|s| s.span_context.span_id() == wait.parent_span_id)
        .expect("the waiter's load span");
    assert!(
        matches!(load.status, opentelemetry::trace::Status::Error { .. }),
        "{:?}",
        load.status
    );
    assert_eq!(text(load, "error.type").as_deref(), Some("storage"));
}

/// Gate the first acquisition: once it has listed, land a new audit file
/// under `root`, then hold it until another query waits on it. The flag
/// says the frontier has moved.
#[cfg(feature = "testing")]
fn gate_moving_the_frontier(
    querier: &Arc<Querier>,
    root: std::path::PathBuf,
) -> Arc<std::sync::atomic::AtomicBool> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let weak = Arc::downgrade(querier);
    let moved = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&moved);
    querier.set_template_map_acquire_gate(Some(Arc::new(move || {
        if !moved.swap(true, Ordering::SeqCst) {
            audit_file(&root, 1_775_131_080);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(querier) = weak.upgrade()
            && querier.template_map_waits() < 1
        {
            assert!(Instant::now() < deadline, "the second query never waited");
            std::thread::sleep(Duration::from_millis(2));
        }
    })));
    flag
}

/// Two concurrent queries at different frontiers: an audit file lands
/// after the first query listed. The second query waits on the first's
/// acquisition (`wait template_map`), then lists again and acquires itself,
/// refolding the now-stale map (`derive template_map`, reason `stale`)
/// under the same `load template_map`. The first query neither waits nor
/// sees the second's fold.
#[cfg(feature = "testing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_query_behind_another_frontier_waits_then_folds_itself() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), &[mined(TS0)]);
    audit_file(dir.path(), 1_775_127_480);
    let querier = Arc::new(Querier::new(dir.path()));
    let frontier_moved = gate_moving_the_frontier(&querier, dir.path().to_path_buf());
    // The second query starts only once the first holds the acquisition and
    // the frontier has moved, so it lists the newer frontier.
    let first = tokio::spawn(query(Arc::clone(&querier)).with_subscriber(dispatch.clone()));
    while !frontier_moved.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let second = tokio::spawn(query(Arc::clone(&querier)).with_subscriber(dispatch.clone()));
    for task in [first, second] {
        task.await.expect("query task");
    }
    querier.set_template_map_acquire_gate(None);

    let spans = exporter.get_finished_spans().expect("spans");
    let children = |load: &SpanData, name: &str| {
        let id = load.span_context.span_id();
        named(&spans, name)
            .into_iter()
            .filter(|s| s.parent_span_id == id)
            .count()
    };
    let loads = named(&spans, "load template_map");
    let [waiter] = loads
        .iter()
        .copied()
        .filter(|l| children(l, "wait template_map") > 0)
        .collect::<Vec<_>>()[..]
    else {
        panic!("exactly one query waited: {loads:?}");
    };
    assert_eq!(
        children(waiter, "wait template_map"),
        1,
        "one wait per loop"
    );
    assert_eq!(
        children(waiter, "derive template_map"),
        1,
        "then its own fold"
    );
    assert_eq!(
        text(waiter, semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME).as_deref(),
        Some("stale")
    );
    let first = loads
        .iter()
        .find(|l| l.span_context.span_id() != waiter.span_context.span_id())
        .expect("the first query's load");
    assert_eq!(children(first, "wait template_map"), 0);
    assert_eq!(children(first, "derive template_map"), 1);
}
