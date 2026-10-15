//! RFC 0038 §3.7.1–§3.7.3 — file-set resolution is a `resolve files` span
//! under the query's span, carrying the tenant and the four work counts (on
//! the error path too), emitted even when the tenant has nothing live, with
//! its S3 requests as `CLIENT` children.
//!
//! The listing runs on the blocking pool, where `spawn_blocking_phase`
//! re-enters the phase span — which is what lets a scoped subscriber see it.

use std::sync::Arc;

use opentelemetry::Value;
use opentelemetry::trace::{SpanId, SpanKind, TraceContextExt as _, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PromotedAttributes, Store, StoreConfig};
use ourios_querier::Querier;
use ourios_semconv as semconv;
use tracing::Instrument as _;
use tracing::instrument::WithSubscriber as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::prelude::*;

use crate::common::{
    DEFAULT_WINDOW_NS, HOUR_NS, NOW, TS0, localstack_s3_config, simple, write_all, write_all_in,
};

const TENANT: &str = "acme";

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

fn named<'s>(spans: &'s [SpanData], name: &str) -> Vec<&'s SpanData> {
    spans.iter().filter(|s| s.name.as_ref() == name).collect()
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

fn text(span: &SpanData, key: &str) -> Option<String> {
    attribute(span, key).map(|v| v.as_str().into_owned())
}

/// The four RFC 0038 §3.7.2 counts on a `resolve files` span, in the order
/// list requests, listed objects, manifest reads, live files.
fn counts(span: &SpanData) -> [Option<Value>; 4] {
    [
        semconv::OURIOS_FILE_SET_LIST_REQUEST_COUNT,
        semconv::OURIOS_FILE_SET_LISTED_OBJECTS,
        semconv::OURIOS_FILE_SET_MANIFEST_READ_COUNT,
        semconv::OURIOS_FILE_SET_LIVE_FILES,
    ]
    .map(|key| attribute(span, key))
}

/// `Some(I64)` of each count, for comparing with [`counts`].
fn expected(values: [i64; 4]) -> [Option<Value>; 4] {
    values.map(|v| Some(Value::I64(v)))
}

/// One query under a stand-in `POST /v1/query` span (the real one is
/// `ourios-server`'s); the stand-in's span id.
async fn query(querier: Arc<Querier>) -> SpanId {
    query_window(querier, NOW, DEFAULT_WINDOW_NS).await
}

/// [`query`] over the window of `window_ns` ending at `now`.
async fn query_window(querier: Arc<Querier>, now: u64, window_ns: u64) -> SpanId {
    let span = tracing::info_span!("POST /v1/query");
    let id = span.context().span().span_context().span_id();
    let parsed = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
    querier
        .run_query(&parsed, &TenantId::new(TENANT), now, window_ns, None)
        .instrument(span)
        .await
        .expect("query");
    id
}

/// A query over a live file set opens exactly one `resolve files` span,
/// `INTERNAL`, under the query, with the tenant and the resolved count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolution_spans_the_file_set_under_the_query() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(dir.path(), &[simple(TENANT, 1, TS0)]);
    let querier = Arc::new(Querier::new(dir.path()));
    let query_id = query(querier).with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!(
            "one resolve span: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert_eq!(resolve.span_kind, SpanKind::Internal);
    assert_eq!(
        resolve.parent_span_id, query_id,
        "the phase nests under the query"
    );
    assert_eq!(
        text(resolve, semconv::OURIOS_TENANT).as_deref(),
        Some(TENANT)
    );
    assert_eq!(
        attribute(resolve, semconv::OURIOS_FILE_SET_LIVE_FILES),
        Some(Value::I64(1))
    );
    // The window cuts `year=2026`, `month=04` and `day=02` takes it whole:
    // the tenant directory and those four are read, and only the one data
    // file is an object. No manifest.
    assert_eq!(counts(resolve), expected([5, 1, 0, 1]));
}

/// A tenant with nothing live still gets the span — the listing ran — with
/// the count at zero (RFC 0038 §3.7.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_file_set_still_spans_the_resolution() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    let querier = Arc::new(Querier::new(dir.path()));
    let query_id = query(querier).with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!("one resolve span over an empty tenant");
    };
    assert_eq!(resolve.parent_span_id, query_id);
    assert_eq!(
        attribute(resolve, semconv::OURIOS_FILE_SET_LIVE_FILES),
        Some(Value::I64(0))
    );
    assert_eq!(
        counts(resolve),
        expected([1, 0, 0, 0]),
        "one read of the absent tenant directory"
    );
    assert!(
        named(&spans, "infer schema").is_empty(),
        "no live file, so no footer to infer from"
    );
}

/// The footer reads between resolution and the scan are one `infer schema`
/// span under the query, with the tenant, after `resolve files` ends and
/// before the scan (RFC 0038 §3.7.1, 2026-10-15 amendment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_inference_spans_the_gap_between_resolution_and_the_scan() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(dir.path(), &[simple(TENANT, 1, TS0)]);
    let querier = Arc::new(Querier::new(dir.path()));
    let query_id = query(querier).with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!("one resolve span");
    };
    let [infer] = named(&spans, "infer schema")[..] else {
        panic!(
            "one infer span: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert_eq!(infer.span_kind, SpanKind::Internal);
    assert_eq!(infer.parent_span_id, query_id, "a sibling of resolve files");
    assert_eq!(text(infer, semconv::OURIOS_TENANT).as_deref(), Some(TENANT));
    assert!(matches!(infer.status, opentelemetry::trace::Status::Unset));
    assert!(resolve.end_time <= infer.start_time);
    // The operator spans go to the global tracer, which this scoped harness
    // does not install; the render pass's map load comes after the scan.
    let [load] = named(&spans, "load template_map")[..] else {
        panic!("one render-time map load");
    };
    assert!(infer.end_time <= load.start_time);
}

/// A data file whose footer does not parse fails the inference: its span
/// is `Error` with `error.type`, and the resolution, which read only
/// manifests, stays `Unset`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_inference_marks_its_phase_as_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(dir.path(), &[simple(TENANT, 1, TS0)]);
    assert!(
        corrupt_data_files(dir.path()) > 0,
        "the fixture wrote a data file"
    );
    let spans = failed_query_spans(dir.path(), "a corrupt footer").await;

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!("one resolve span");
    };
    assert!(matches!(
        resolve.status,
        opentelemetry::trace::Status::Unset
    ));
    let [infer] = named(&spans, "infer schema")[..] else {
        panic!(
            "one infer span for the failed inference: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert!(
        matches!(infer.status, opentelemetry::trace::Status::Error { .. }),
        "{:?}",
        infer.status
    );
    assert_eq!(text(infer, "error.type").as_deref(), Some("storage"));
}

/// Overwrite every data file under `root` with bytes that are not Parquet,
/// leaving the manifests that list them intact.
fn corrupt_data_files(root: &std::path::Path) -> usize {
    let files = parquet_files(root);
    for file in &files {
        std::fs::write(file, b"not parquet").expect("corrupt data file");
    }
    files.len()
}

/// The spans of one query over `root` that `fault` makes fail.
async fn failed_query_spans(root: &std::path::Path, fault: &str) -> Vec<SpanData> {
    let (exporter, _provider, dispatch) = traced();
    let querier = Querier::new(root);
    let result = async {
        let parsed = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
        querier
            .run_query(
                &parsed,
                &TenantId::new(TENANT),
                NOW,
                DEFAULT_WINDOW_NS,
                None,
            )
            .instrument(tracing::info_span!("POST /v1/query"))
            .await
    }
    .with_subscriber(dispatch)
    .await;
    assert!(result.is_err(), "{fault} fails the query");
    exporter.get_finished_spans().expect("spans")
}

/// Every `.parquet` file under `root`.
fn parquet_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                files.push(path);
            }
        }
    }
    files
}

/// Plant a manifest that does not parse in every partition holding data
/// under `root`, so file-set resolution fails on its first manifest read.
fn corrupt_manifests(root: &std::path::Path) -> usize {
    let partitions: std::collections::BTreeSet<_> = parquet_files(root)
        .iter()
        .filter_map(|file| file.parent().map(std::path::Path::to_path_buf))
        .collect();
    for dir in &partitions {
        std::fs::write(
            dir.join(ourios_parquet::MANIFEST_FILENAME),
            b"not a manifest",
        )
        .expect("corrupt manifest");
    }
    partitions.len()
}

/// A resolution that fails sets the phase's status to `Error` with
/// `error.type` and records no live-file count (RFC 0038 §3.7.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_resolution_marks_the_phase_as_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(dir.path(), &[simple(TENANT, 1, TS0)]);
    assert!(
        corrupt_manifests(dir.path()) > 0,
        "the fixture wrote a partition"
    );
    let spans = failed_query_spans(dir.path(), "a corrupt manifest").await;

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!(
            "one resolve span for the failed resolution: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert!(
        matches!(resolve.status, opentelemetry::trace::Status::Error { .. }),
        "{:?}",
        resolve.status
    );
    assert_eq!(text(resolve, "error.type").as_deref(), Some("storage"));
    assert_eq!(
        attribute(resolve, semconv::OURIOS_FILE_SET_LIVE_FILES),
        None
    );
    // The work done up to the failure is still counted (§3.7.2): the five
    // directory reads down to `hour=10`, its data file and manifest, and the
    // one manifest read that failed.
    assert_eq!(
        counts(resolve)[..3],
        expected([5, 2, 1, 0])[..3],
        "the counts are set on the error path"
    );
}

/// A narrow window leaves out the partitions it cannot reach, and the
/// manifest of the one it does retires a file: what was listed exceeds what
/// is live, and each count is the work the window walk did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_narrow_window_counts_only_the_partitions_it_reaches() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    seed_narrow(&Store::local(dir.path()).expect("local store"));
    let querier = Arc::new(Querier::new(dir.path()));
    narrow_query(querier).with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!("one resolve span");
    };
    assert_eq!(counts(resolve), narrow_counts());
}

/// RFC 0038 §3.7.3 on S3 (#986): the resolution's listings and manifest GET
/// are `CLIENT` spans under `resolve files`, one per counted call, and the
/// counts match the local walk's over the same tree. Run by the
/// `s3-integration` CI job by exact name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC 0038 §3.7.3 S3 arm; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn s3_resolution_requests_nest_under_the_phase() {
    let (exporter, provider, dispatch) = traced();
    // The store's `CLIENT` spans go to the global tracer provider.
    opentelemetry::global::set_tracer_provider(provider.clone());
    let (_container, config) = localstack_s3_config("ourios-it-file-set").await;
    let store = Store::s3(config.clone()).expect("s3 store");
    tokio::task::spawn_blocking(move || seed_narrow(&store))
        .await
        .expect("seed");
    let querier = Querier::from_store_config(&StoreConfig::S3(config)).expect("s3 querier");
    let query_id = narrow_query(Arc::new(querier))
        .with_subscriber(dispatch)
        .await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!("one resolve span");
    };
    assert_eq!(resolve.parent_span_id, query_id);
    assert_eq!(counts(resolve), narrow_counts());
    let under_resolve = |name: &str| {
        spans
            .iter()
            .filter(|s| {
                s.span_kind == SpanKind::Client
                    && s.parent_span_id == resolve.span_context.span_id()
                    && s.name.as_ref() == name
            })
            .count()
    };
    let [list_requests, _, manifest_reads, _] = narrow_counts();
    assert_eq!(
        Some(Value::I64(
            i64::try_from(under_resolve("S3.ListObjectsV2")).expect("count")
        )),
        list_requests,
        "one ListObjectsV2 per listing call (each under one page)"
    );
    assert_eq!(
        Some(Value::I64(
            i64::try_from(under_resolve("S3.GetObject")).expect("count")
        )),
        manifest_reads,
    );
}

/// Two files in 2026-04-02 hour 10, one in hour 8, one on 2026-03-30, and a
/// manifest in hour 10 retiring one of its two.
fn seed_narrow(store: &Store) {
    let promoted = PromotedAttributes::default();
    write_all_in(store, &[simple(TENANT, 1, TS0)], &promoted);
    write_all_in(
        store,
        &[
            simple(TENANT, 1, TS0),
            simple(TENANT, 1, TS0 - 2 * HOUR_NS),
            simple(TENANT, 1, TS0 - 70 * HOUR_NS),
        ],
        &promoted,
    );
    retire_all_but_one(store, "hour=10");
}

/// The [`seed_narrow`] tree's query: TS0 is 10:58, so `[10:29, 10:59)`
/// reaches hour 10 alone.
async fn narrow_query(querier: Arc<Querier>) -> SpanId {
    query_window(querier, TS0 + 60_000_000_000, HOUR_NS / 2).await
}

/// The counts [`narrow_query`] makes over [`seed_narrow`] on either backend:
/// the tenant, `year=2026`, `month=04`, `day=02` and `hour=10` are listed
/// (`month=03` and `hour=08` are not); `hour=10` holds two data files and a
/// manifest naming one of them.
fn narrow_counts() -> [Option<Value>; 4] {
    expected([5, 3, 1, 1])
}

/// Plant a manifest in the partition whose key contains `hour`, naming only
/// one of its data files and retiring the rest.
fn retire_all_but_one(store: &Store, hour: &str) {
    let keys: Vec<String> = store
        .list_with_sizes_blocking(Some("data"))
        .expect("list data")
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| key.contains(hour) && key.ends_with(".parquet"))
        .collect();
    let [kept, _, ..] = &keys[..] else {
        panic!("two data files in {hour}: {keys:?}");
    };
    let (partition, name) = kept.rsplit_once('/').expect("a partition key");
    let manifest = ourios_parquet::Manifest {
        generation: 1,
        files: vec![name.to_owned()],
    };
    store
        .put_blocking(
            &format!("{partition}/{}", ourios_parquet::MANIFEST_FILENAME),
            manifest.to_json().expect("manifest json"),
        )
        .expect("put manifest");
}

/// The phase closes at the finished URLs (RFC 0038 §3.7.1), not when the
/// query ends: it has ended before the render pass opens `load template_map`
/// after the scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_resolution_phase_ends_before_the_render_pass() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(dir.path(), &[simple(TENANT, 1, TS0)]);
    let querier = Arc::new(Querier::new(dir.path()));
    query(querier).with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!("one resolve span");
    };
    let [load] = named(&spans, "load template_map")[..] else {
        panic!(
            "one render-time map load: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert!(
        resolve.end_time <= load.start_time,
        "resolve files must end before the render pass loads the map",
    );
}
