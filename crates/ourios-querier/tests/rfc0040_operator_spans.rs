//! RFC 0040 — a real query emits an operator span tree
//! (`ourios_df_otel::record_plan_spans`, wired at the `scan_stats` call
//! sites) whose spans nest under the current tracing span.
//!
//! `record_operator_spans` resolves its tracer via the process-global
//! `opentelemetry::global::tracer` (matching real `ourios_telemetry::init`
//! wiring), so this test installs a process-global `TracerProvider` — the
//! `rfc0016_6_query_metrics` / `rfc0033_7_observability.rs` precedent,
//! applied to traces — and lives in its own integration binary (its own
//! process), the RFC0028.2 process-isolation exemption. Both scenarios run
//! sequentially inside one `#[tokio::test]` rather than as separate test
//! functions, so nothing within this binary races the global provider
//! either.
//!
//! `ourios-querier` has no span of its own (the `POST /v1/query` span is
//! `ourios-server`'s, RFC 0038/0039); a plain `tracing::info_span!` stands
//! in for it here, exercising the same span-nesting mechanism
//! (`OpenTelemetrySpanExt`/`tracing-opentelemetry`) the real request path
//! uses. Harness follows `ourios-server`'s
//! `tests/it/rfc0039_1_query_propagation.rs`: an `InMemorySpanExporter`
//! captures spans off a scoped `SdkTracerProvider`, driven via
//! `tracing_opentelemetry::layer()` + `.with_subscriber(...)`.

use std::collections::HashMap;
use std::path::Path;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use ourios_querier::{Querier, QueryRequest};
use tracing::Instrument as _;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;

fn simple(tenant: &str, template_id: u64, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
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
    }
}

fn write_all(bucket: &Path, recs: &[MinedRecord]) {
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

fn req(tenant: &str, template_id: u64, limit: usize) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new(tenant),
        time_range: None,
        template_id: Some(template_id),
        severity_text: None,
        limit: Some(limit),
    }
}

/// Drive one `Querier::run` under a synthetic root span + a scoped `OTel`
/// pipeline, and return the exported spans. Mirrors
/// `rfc0039_1_query_propagation.rs::query_spans`, minus the HTTP layer: the
/// test supplies the parent span `record_operator_spans` nests under.
async fn run_traced_query(
    bucket: &Path,
    tenant: &str,
    template_id: u64,
    limit: usize,
) -> Vec<opentelemetry_sdk::trace::SpanData> {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")));

    opentelemetry::global::set_tracer_provider(provider.clone());

    let querier = Querier::new(bucket);
    let request = req(tenant, template_id, limit);

    async {
        let query_span = tracing::info_span!("POST /v1/query");
        querier
            .run(request)
            .instrument(query_span)
            .await
            .expect("query succeeds")
    }
    .with_subscriber(subscriber)
    .await;

    provider.force_flush().expect("flush");
    exporter.get_finished_spans().expect("exported spans")
}

/// RFC0040.1/.2/.3: a real query's operator span tree nests under the query
/// span, with correct attributes and real (non-inverted) timestamps.
async fn assert_operator_span_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(
        dir.path(),
        &[
            simple("acme", 7, TS0),
            simple("acme", 7, TS0 + 1_000_000),
            simple("acme", 7, TS0 + 2_000_000),
            // a filler template in a distinct hour, so the target query still
            // has a second file to prune (a genuine, if tiny, scan shape).
            simple("acme", 8, TS0 + HOUR_NS),
        ],
    );

    let spans = run_traced_query(dir.path(), "acme", 7, 10).await;
    let names: Vec<_> = spans.iter().map(|s| s.name.to_string()).collect();

    let root = spans
        .iter()
        .find(|s| s.name == "POST /v1/query")
        .expect("the stand-in query span itself is exported");

    // Only the plan's root node is a *direct* child of the query span; a
    // leaf like `DataSourceExec` nests under its own parent operator (e.g.
    // `RepartitionExec`) — so parentage is checked by walking span -> parent
    // -> parent's parent up to the query span (RFC0040.1: "whose parent
    // (transitively) is the `POST /v1/query` span").
    let operator_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() != "POST /v1/query")
        .collect();
    assert!(
        !operator_spans.is_empty(),
        "expected at least one operator span, got: {names:?}"
    );
    let by_span_id: HashMap<_, _> = spans
        .iter()
        .map(|s| (s.span_context.span_id(), s))
        .collect();
    for op in &operator_spans {
        assert_eq!(
            op.span_context.trace_id(),
            root.span_context.trace_id(),
            "operator span {:?} must share the query span's trace",
            op.name,
        );
        let mut ancestor = op.parent_span_id;
        let mut hops = 0;
        while ancestor != root.span_context.span_id() {
            hops += 1;
            assert!(
                hops <= names.len(),
                "operator span {:?} never reaches the query span walking parents: {names:?}",
                op.name,
            );
            let parent = by_span_id.get(&ancestor).unwrap_or_else(|| {
                panic!(
                    "operator span {:?}'s parent {ancestor} was not exported",
                    op.name
                )
            });
            ancestor = parent.parent_span_id;
        }
    }

    // RFC0040.3 — attribute shape for a scan-shaped node (pruning counts)
    // and a non-scan node (rows/bytes/elapsed_compute only).
    let scan = operator_spans
        .iter()
        .find(|s| {
            s.attributes
                .iter()
                .any(|kv| kv.key.as_str() == "datafusion.operator.row_groups_matched")
        })
        .unwrap_or_else(|| panic!("expected a scan node reporting pruning counts among {names:?}"));
    assert!(
        scan.attributes
            .iter()
            .any(|kv| kv.key.as_str() == "datafusion.operator.row_groups_pruned"),
        "the scan node must also report row_groups_pruned"
    );
    let non_scan = operator_spans
        .iter()
        .find(|s| {
            !s.attributes
                .iter()
                .any(|kv| kv.key.as_str() == "datafusion.operator.row_groups_matched")
                && !s.attributes.is_empty()
        })
        .unwrap_or_else(|| panic!("expected a non-scan node with attributes among {names:?}"));
    assert!(
        non_scan
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == "datafusion.operator.output_rows"),
        "a non-scan node must still report output_rows"
    );

    // RFC0040.2 — real wall-clock bounds, never inverted.
    for op in &operator_spans {
        assert!(
            op.start_time <= op.end_time,
            "{:?} start must not be after its end",
            op.name
        );
    }
}

/// RFC0040.5 — span count is bounded by plan node count, independent of the
/// number of records the query returns.
async fn assert_span_count_independent_of_record_count() {
    let dir_few = tempfile::tempdir().expect("tempdir");
    let few: Vec<_> = (0..3)
        .map(|i| simple("acme", 9, TS0 + i * 1_000_000))
        .collect();
    write_all(dir_few.path(), &few);
    let spans_few = run_traced_query(dir_few.path(), "acme", 9, 100).await;

    let dir_many = tempfile::tempdir().expect("tempdir");
    let many: Vec<_> = (0..500)
        .map(|i| simple("acme", 9, TS0 + i * 1_000_000))
        .collect();
    write_all(dir_many.path(), &many);
    let spans_many = run_traced_query(dir_many.path(), "acme", 9, 100).await;

    let count_few = spans_few
        .iter()
        .filter(|s| s.name != "POST /v1/query")
        .count();
    let count_many = spans_many
        .iter()
        .filter(|s| s.name != "POST /v1/query")
        .count();
    assert_eq!(
        count_few, count_many,
        "operator span count must track the plan shape, not the row count"
    );
}

/// Both scenarios run sequentially inside one `#[tokio::test]` — not as
/// separate test functions — so nothing in this process races the
/// process-global tracer provider `run_traced_query` installs (see the
/// module doc comment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0040_operator_spans() {
    assert_operator_span_tree().await;
    assert_span_count_independent_of_record_count().await;
}
