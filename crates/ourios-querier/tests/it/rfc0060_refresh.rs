//! RFC 0060 — the background template-map refresh, driven through the
//! `testing` hooks on a local store: single passes, the single-flight's
//! acquisition and join counters, and an acquisition gate.
//!
//! Spans go to a scoped subscriber over each test's own tracer provider;
//! the metric halves of these scenarios are in
//! `tests/rfc0060_refresh_observability.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::trace::{SpanId, SpanKind, Status, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use ourios_core::tenant::TenantId;
use ourios_querier::{Querier, QueryError, QueryResult, TEMPLATE_MAP_FILENAME};
use ourios_semconv as semconv;
use tracing::Instrument as _;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

use crate::common::{HOUR_NS, TS0, adopted, simple, write_all, write_audit};

const NOW: u64 = TS0 + 24 * HOUR_NS;
const WINDOW_NS: u64 = 30 * 24 * HOUR_NS;

/// A store where each of `tenants` has one audit file and one data row.
fn store(tenants: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for tenant in tenants {
        write_audit(dir.path(), &[adopted(tenant, 1, TS0)]);
        write_all(dir.path(), &[simple(tenant, 1, TS0)]);
    }
    dir
}

fn tenant_dir(root: &Path, tenant: &str) -> PathBuf {
    root.join("audit").join(format!("tenant_id={tenant}"))
}

/// A body-rendering query for `tenant`, which acquires its template map.
async fn query(querier: &Querier, tenant: &str) -> Result<QueryResult, QueryError> {
    let parsed = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
    querier
        .run_query(&parsed, &TenantId::new(tenant), NOW, WINDOW_NS, None)
        .await
}

/// The tenants acquired so far, in order.
fn acquired(querier: &Querier) -> Vec<String> {
    querier
        .template_map_acquisitions()
        .into_iter()
        .map(|(tenant, _)| tenant)
        .collect()
}

/// Wait (bounded) until `done` holds.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Hold every acquisition until `released`.
fn gate_until(querier: &Querier, released: &Arc<AtomicBool>) {
    let released = Arc::clone(released);
    querier.set_template_map_acquire_gate(Some(Arc::new(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !released.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "never released");
            std::thread::sleep(Duration::from_millis(2));
        }
    })));
}

/// Rewrite the published artifact under `root` for `tenant`, editing its
/// JSON body.
fn rewrite_artifact(root: &Path, tenant: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = tenant_dir(root, tenant).join(TEMPLATE_MAP_FILENAME);
    let compressed = std::fs::read(&path).expect("read artifact");
    let json = zstd::decode_all(compressed.as_slice()).expect("decompress artifact");
    let mut value: serde_json::Value = serde_json::from_slice(&json).expect("artifact json");
    edit(&mut value);
    let json = serde_json::to_vec(&value).expect("serialize artifact");
    let compressed = zstd::encode_all(json.as_slice(), 3).expect("compress artifact");
    std::fs::write(path, compressed).expect("write artifact");
}

/// Overwrite every audit Parquet file of `tenant` with bytes no reader
/// accepts.
fn corrupt_audit(root: &Path, tenant: &str) {
    let mut dirs = vec![tenant_dir(root, tenant)];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                std::fs::write(&path, b"not parquet").expect("corrupt");
            }
        }
    }
}

/// Make the tenant listing fail: `audit` becomes a file.
fn break_listing(root: &Path) {
    std::fs::rename(root.join("audit"), root.join("audit.moved")).expect("move audit/");
    std::fs::write(root.join("audit"), b"not a directory").expect("plant a file");
}

fn restore_listing(root: &Path) {
    std::fs::remove_file(root.join("audit")).expect("remove the file");
    std::fs::rename(root.join("audit.moved"), root.join("audit")).expect("restore audit/");
}

/// A scoped tracing pipeline over its own in-memory span exporter. Keep
/// the provider alive until the spans are read.
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

fn text(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

fn id(span: &SpanData) -> SpanId {
    span.span_context.span_id()
}

/// The one `refresh template_map` span exported, asserted a root
/// `INTERNAL` span.
fn the_pass(spans: &[SpanData]) -> &SpanData {
    let [pass] = named(spans, "refresh template_map")[..] else {
        panic!(
            "one pass span: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert_eq!(pass.span_kind, SpanKind::Internal);
    assert_eq!(
        pass.parent_span_id,
        SpanId::INVALID,
        "the pass is a root span"
    );
    pass
}

/// The `load template_map` span for `tenant`.
fn load_of<'s>(spans: &'s [SpanData], tenant: &str) -> &'s SpanData {
    named(spans, "load template_map")
        .into_iter()
        .find(|s| text(s, semconv::OURIOS_TENANT).as_deref() == Some(tenant))
        .unwrap_or_else(|| panic!("a load span for {tenant}"))
}

/// Scenario RFC0060.1 — a start pass absorbs the bootstrap.
/// Scenario RFC0060.8 — its pass is one root `refresh template_map` span
/// with the tenant's `load template_map` beneath it (the metric halves are
/// in `rfc0060_refresh_observability`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0060_1_a_start_pass_absorbs_the_bootstrap() {
    let (exporter, _provider, dispatch) = traced();
    let dir = store(&["acme"]);
    let querier = Querier::new(dir.path());
    query(&querier, "acme")
        .await
        .expect("the cold query publishes");
    rewrite_artifact(dir.path(), "acme", |json| {
        json.as_object_mut().expect("object").remove("fold_state");
    });
    write_audit(dir.path(), &[adopted("acme", 2, TS0 + HOUR_NS)]);
    exporter.reset();

    // Run inside a live span of the same pipeline: the pass must still be a
    // root, not that span's child.
    let caller = tracing::dispatcher::with_default(&dispatch, || {
        tracing::info_span!("an unrelated caller span")
    });
    let mut refresher = querier.template_map_refresher();
    refresher
        .run_pass()
        .instrument(caller)
        .with_subscriber(dispatch.clone())
        .await;
    let spans = exporter.get_finished_spans().expect("spans");
    let pass = the_pass(&spans);
    let load = load_of(&spans, "acme");
    assert_eq!(
        load.parent_span_id,
        id(pass),
        "the load nests under the pass"
    );
    assert_eq!(
        text(load, semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME).as_deref(),
        Some("stale")
    );
    let [derive] = named(&spans, "derive template_map")[..] else {
        panic!("the pass ran the full fold");
    };
    assert_eq!(derive.parent_span_id, id(load));

    exporter.reset();
    let rows = query(&querier, "acme")
        .with_subscriber(dispatch)
        .await
        .expect("query")
        .rows;
    assert_eq!(rows, 1);
    let spans = exporter.get_finished_spans().expect("spans");
    assert_eq!(
        text(
            load_of(&spans, "acme"),
            semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME
        )
        .as_deref(),
        Some("hit"),
        "the query after the pass hits"
    );
    assert!(
        named(&spans, "derive template_map").is_empty(),
        "the query folds nothing"
    );
}

/// Scenario RFC0060.2 — a query during a background fold joins it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0060_2_a_query_during_a_background_fold_joins_it() {
    let dir = store(&["acme"]);
    let querier = Arc::new(Querier::new(dir.path()));
    let weak = Arc::downgrade(&querier);
    querier.set_template_map_acquire_gate(Some(Arc::new(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(querier) = weak.upgrade()
            && querier.template_map_waits() < 1
        {
            assert!(Instant::now() < deadline, "the query never waited");
            std::thread::sleep(Duration::from_millis(2));
        }
    })));
    let mut refresher = querier.template_map_refresher();
    let pass = tokio::spawn(async move { refresher.run_pass().await });
    eventually("the task acquires", || acquired(&querier).len() == 1).await;

    let result = query(&querier, "acme").await.expect("the joined query");
    pass.await.expect("pass task");

    assert_eq!(
        acquired(&querier),
        vec!["acme"],
        "exactly one fold, the task's"
    );
    assert_eq!(querier.template_map_joins(), 1, "the query joined it");
    assert_eq!(result.rows, 1);
    assert!(
        result.registry_bytes_read > 0,
        "it reports the task's acquisition"
    );
}

/// Scenario RFC0060.3 — one background acquisition at a time, in listing
/// order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0060_3_one_background_acquisition_at_a_time() {
    let dir = store(&["t3", "t1", "t2"]);
    let querier = Querier::new(dir.path());
    querier.set_template_map_acquire_gate(Some(Arc::new(|| {
        std::thread::sleep(Duration::from_millis(20));
    })));
    querier.template_map_refresher().run_pass().await;

    assert_eq!(
        querier.template_map_acquisitions(),
        vec![
            ("t1".to_owned(), 1),
            ("t2".to_owned(), 1),
            ("t3".to_owned(), 1)
        ],
        "listing order, and each the only acquisition in flight",
    );
}

/// Scenario RFC0060.4 — failure is contained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0060_4_failure_is_contained() {
    let (exporter, _provider, dispatch) = traced();
    let dir = store(&["a", "b"]);
    corrupt_audit(dir.path(), "a");
    let querier = Querier::new(dir.path());
    let mut refresher = querier.template_map_refresher();

    refresher.run_pass().with_subscriber(dispatch.clone()).await;
    let spans = exporter.get_finished_spans().expect("spans");
    let pass = the_pass(&spans);
    assert_eq!(pass.status, Status::Unset);
    assert_eq!(text(pass, "error.type"), None);
    let (failed, fine) = (load_of(&spans, "a"), load_of(&spans, "b"));
    assert!(matches!(failed.status, Status::Error { .. }));
    assert_eq!(text(failed, "error.type").as_deref(), Some("storage"));
    assert_eq!(fine.status, Status::Unset);
    assert_eq!(failed.parent_span_id, id(pass));
    assert_eq!(fine.parent_span_id, id(pass));

    // The next pass runs; the failed tenant sits it out (RFC0060.10).
    exporter.reset();
    refresher.run_pass().with_subscriber(dispatch.clone()).await;
    let spans = exporter.get_finished_spans().expect("spans");
    let pass = the_pass(&spans);
    assert_eq!(pass.status, Status::Unset);
    let [load] = named(&spans, "load template_map")[..] else {
        panic!("only the healthy tenant is acquired");
    };
    assert_eq!(text(load, semconv::OURIOS_TENANT).as_deref(), Some("b"));
    assert_eq!(load.parent_span_id, id(pass));

    break_listing(dir.path());
    exporter.reset();
    refresher.run_pass().with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");
    let pass = the_pass(&spans);
    assert!(matches!(pass.status, Status::Error { .. }));
    assert_eq!(text(pass, "error.type").as_deref(), Some("storage"));
    assert!(
        named(&spans, "load template_map").is_empty(),
        "a failed listing ends the pass before any tenant"
    );
}

/// A shutdown signal: `fire` completes `signal()`, before or after it is
/// first polled.
fn shutdown() -> (Arc<tokio::sync::Notify>, impl Future<Output = ()>) {
    let notify = Arc::new(tokio::sync::Notify::new());
    let waiter = Arc::clone(&notify);
    (notify, async move { waiter.notified().await })
}

/// Scenario RFC0060.7 — shutdown is prompt: between tenants, and while
/// waiting on the interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0060_7_shutdown_between_tenants_or_idle_is_prompt() {
    let dir = store(&["a", "b"]);
    let querier = Querier::new(dir.path());
    let (stop, signal) = shutdown();
    let fire = Arc::clone(&stop);
    querier.set_template_map_acquire_gate(Some(Arc::new(move || fire.notify_one())));
    let run = querier
        .template_map_refresher()
        .run(Duration::from_secs(3_600), signal);
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the task exits");
    assert_eq!(
        acquired(&querier),
        vec!["a"],
        "no acquisition after shutdown"
    );

    let querier = Querier::new(dir.path());
    let refresher = querier.template_map_refresher();
    let log = refresher.pass_log();
    let (stop, signal) = shutdown();
    let run = tokio::spawn(refresher.run(Duration::from_secs(3_600), signal));
    eventually("the first pass finishes", || {
        log.passes().first().is_some_and(|pass| pass.1.is_some())
    })
    .await;
    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the idle task exits")
        .expect("task");
    assert_eq!(log.passes().len(), 1);
}

/// Scenario RFC0060.7 — a pass whose acquisition is gated on the blocking
/// pool exits on shutdown without waiting for it, and the flight still
/// completes and publishes once the gate opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0060_7_shutdown_leaves_a_blocked_flight_to_finish() {
    let dir = store(&["acme"]);
    let querier = Querier::new(dir.path());
    let released = Arc::new(AtomicBool::new(false));
    gate_until(&querier, &released);
    let (stop, signal) = shutdown();
    let run = tokio::spawn(
        querier
            .template_map_refresher()
            .run(Duration::from_secs(3_600), signal),
    );
    eventually("the task acquires", || acquired(&querier).len() == 1).await;
    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the task exits while its fold is gated")
        .expect("task");

    let artifact = tenant_dir(dir.path(), "acme").join(TEMPLATE_MAP_FILENAME);
    assert!(!artifact.exists(), "the gated fold has not published yet");
    released.store(true, Ordering::SeqCst);
    eventually("the abandoned flight publishes", || artifact.exists()).await;
}

/// Scenario RFC0060.7 — shutdown during a slow tenant listing: the task
/// exits without waiting for the LIST, which finishes on the blocking pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0060_7_shutdown_does_not_wait_for_a_slow_listing() {
    let dir = store(&["acme"]);
    let querier = Querier::new(dir.path());
    let released = Arc::new(AtomicBool::new(false));
    let listing = Arc::new(AtomicBool::new(false));
    let gate = {
        let (released, listing) = (Arc::clone(&released), Arc::clone(&listing));
        Arc::new(move || {
            listing.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(30);
            while !released.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline, "never released");
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let (stop, signal) = shutdown();
    let run = tokio::spawn(
        querier
            .template_map_refresher()
            .with_listing_gate(gate)
            .run(Duration::from_secs(3_600), signal),
    );
    eventually("the pass is listing", || listing.load(Ordering::SeqCst)).await;
    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the task exits while its listing is held")
        .expect("task");
    assert!(acquired(&querier).is_empty(), "no tenant was acquired");
    released.store(true, Ordering::SeqCst);
}

/// Scenario RFC0060.10 — a tenant that persists nothing is backed off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0060_10_a_tenant_that_persists_nothing_is_backed_off() {
    // An audit directory with no audit file: the fold is empty, so the
    // artifact is never smaller than what it caches and the write-through
    // abstains on every acquisition.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tenant_dir(dir.path(), "idle")).expect("tenant dir");
    write_all(dir.path(), &[simple("idle", 1, TS0)]);
    let querier = Querier::new(dir.path());
    let mut refresher = querier.template_map_refresher();

    let mut acquired_on = Vec::new();
    for pass in 1..=26 {
        let before = acquired(&querier).len();
        refresher.run_pass().await;
        if acquired(&querier).len() > before {
            acquired_on.push(pass);
        }
    }
    assert_eq!(acquired_on, vec![1, 3, 7, 15], "then skipped for 11 passes");

    let before = acquired(&querier).len();
    for _ in 0..2 {
        query(&querier, "idle").await.expect("query");
    }
    assert_eq!(
        acquired(&querier).len(),
        before + 2,
        "a query is never skipped"
    );

    // Pass 27 acquires again, now publishing; pass 28 follows at once.
    write_audit(dir.path(), &[adopted("idle", 1, TS0)]);
    refresher.run_pass().await;
    assert!(refresher.backed_off().is_empty(), "a publish resets it");
    let before = acquired(&querier).len();
    refresher.run_pass().await;
    assert_eq!(
        acquired(&querier).len(),
        before + 1,
        "a hit, on the next pass"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tenant_dir(dir.path(), "idle")).expect("tenant dir");
    let querier = Querier::new(dir.path());
    let mut refresher = querier.template_map_refresher();
    refresher.run_pass().await;
    assert_eq!(refresher.backed_off(), vec!["idle"]);
    break_listing(dir.path());
    refresher.run_pass().await;
    assert_eq!(
        refresher.backed_off(),
        vec!["idle"],
        "a failed listing drops nothing"
    );
    restore_listing(dir.path());
    std::fs::remove_dir_all(tenant_dir(dir.path(), "idle")).expect("drop the tenant");
    refresher.run_pass().await;
    assert!(
        refresher.backed_off().is_empty(),
        "an unlisted tenant's counter is dropped"
    );
}

/// Scenario RFC0060.11 — passes are spaced from their end and never
/// overlap, under paused time.
#[tokio::test(start_paused = true)]
async fn rfc0060_11_passes_are_spaced_from_their_end() {
    const INTERVAL: Duration = Duration::from_secs(300);
    let dir = store(&["acme"]);
    let querier = Querier::new(dir.path());
    let released = Arc::new(AtomicBool::new(false));
    gate_until(&querier, &released);
    let refresher = querier.template_map_refresher();
    let log = refresher.pass_log();
    let (stop, signal) = shutdown();
    let run = tokio::spawn(refresher.run(INTERVAL, signal));

    // A gated acquisition keeps the paused clock from advancing on its own,
    // so this polls in real time.
    let poll = async |done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "never happened");
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    poll(&|| acquired(&querier).len() == 1).await;
    tokio::time::advance(2 * INTERVAL).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(log.passes().len(), 1, "no second pass while the first runs");

    released.store(true, Ordering::SeqCst);
    poll(&|| log.passes()[0].1.is_some()).await;
    let first_end = log.passes()[0].1.expect("finished");
    tokio::time::sleep(INTERVAL + Duration::from_secs(1)).await;

    let passes = log.passes();
    assert_eq!(passes.len(), 2, "one more pass, no catch-up");
    assert_eq!(
        passes[1].0 - first_end,
        INTERVAL,
        "spaced from the first's end"
    );
    stop.notify_one();
    run.await.expect("task");
}

/// Scenario RFC0060.8 — Telemetry is registry-conformant.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.8 stub — span shape in stage 1 (with RFC0060.1), weaver live-check of the metrics in stage 2 (the served role)"]
fn rfc0060_8_telemetry_is_registry_conformant() {
    todo!(
        "RFC0060.8 — the pass is one root INTERNAL `refresh template_map` span over \
         each tenant's `load template_map`, and `acquire.trigger` and \
         `refresh.duration` pass weaver live-check against ourios-semconv"
    );
}
