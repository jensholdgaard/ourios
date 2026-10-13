//! RFC 0060 — the metric halves of RFC0060.1, .2, .4 and .8: who started
//! an acquisition (`ourios.template_map.acquire.trigger`) on the lookup and
//! derive instruments, and one `ourios.template_map.refresh.duration`
//! sample per pass. Instrument and attribute names come from the
//! weaver-generated `ourios-semconv` constants.
//!
//! Installs a process-global in-memory `MeterProvider`, so it is its own
//! binary (an RFC0028.2 process-isolation exemption, as
//! `rfc0033_7_observability.rs`). Its acts share the cumulative
//! instruments, so each asserts only what it added.

#![cfg(feature = "testing")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, Metric, MetricData, ResourceMetrics, ScopeMetrics,
};
use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink, ParamType, TemplateChange};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{ParquetAuditSink, PartitionKey, Store, Writer};
use ourios_querier::{Querier, TEMPLATE_MAP_FILENAME};
use ourios_semconv as semconv;
use ourios_telemetry::TelemetryGuard;

/// 2026-04-02T10:58:00 UTC, the querier suite's fixture instant.
const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;
const NOW: u64 = TS0 + 24 * HOUR_NS;
const WINDOW_NS: u64 = 30 * 24 * HOUR_NS;

fn adopted(tenant: &str, template_id: u64, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: std::time::UNIX_EPOCH + Duration::from_nanos(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: ourios_core::audit::hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Adopted {
                template_version: 1,
                new_template: "user <*>".to_string(),
            },
        },
    }
}

fn mined(tenant: &str, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
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
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

fn write_audit(bucket: &Path, events: &[AuditEvent]) {
    let mut sink = ParquetAuditSink::new(Store::local(bucket).expect("store"));
    for e in events {
        sink.emit(e.clone());
    }
    assert_eq!(sink.write_failures(), 0, "audit fixtures must all persist");
}

/// A store where each of `tenants` has one audit file and one data row.
fn store(tenants: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for tenant in tenants {
        write_audit(dir.path(), &[adopted(tenant, 1, TS0)]);
        let record = mined(tenant, TS0);
        let part = PartitionKey::derive(&record).expect("partition");
        let mut writer = Writer::open(dir.path(), part).expect("writer");
        writer.append_records(&[record]).expect("append");
        writer.close().expect("close");
    }
    dir
}

async fn query(querier: &Querier, tenant: &str) -> u64 {
    let parsed = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
    querier
        .run_query(&parsed, &TenantId::new(tenant), NOW, WINDOW_NS, None)
        .await
        .expect("query")
        .rows
}

/// What the instruments hold: per metric, per sorted `key=value` attribute
/// set, the counter value or the histogram sample count.
type Points = BTreeMap<&'static str, BTreeMap<Vec<String>, u64>>;

const METRICS: [&str; 3] = [
    semconv::OURIOS_TEMPLATE_MAP_LOOKUPS,
    semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION,
    semconv::OURIOS_TEMPLATE_MAP_REFRESH_DURATION,
];

struct Pipeline {
    guard: TelemetryGuard,
    exporter: InMemoryMetricExporter,
}

impl Pipeline {
    fn collection(&self) -> ResourceMetrics {
        self.guard.force_flush().expect("force_flush");
        self.exporter
            .get_finished_metrics()
            .expect("metrics exported")
            .pop()
            .expect("a collection")
    }

    fn points(&self) -> Points {
        let rms = self.collection();
        METRICS
            .into_iter()
            .map(|name| (name, metric(&rms, name).map(points).unwrap_or_default()))
            .collect()
    }

    /// What `act` added to each metric.
    async fn added(&self, act: impl Future<Output = ()>) -> Points {
        let before = self.points();
        act.await;
        let after = self.points();
        after
            .into_iter()
            .map(|(name, points)| {
                let added = points
                    .into_iter()
                    .filter_map(|(key, n)| {
                        let n0 = before[name].get(&key).copied().unwrap_or(0);
                        (n > n0).then(|| (key, n - n0))
                    })
                    .collect();
                (name, added)
            })
            .collect()
    }
}

fn metric<'a>(rms: &'a ResourceMetrics, name: &str) -> Option<&'a Metric> {
    rms.scope_metrics()
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == name)
}

fn points(metric: &Metric) -> BTreeMap<Vec<String>, u64> {
    fn key<'a>(attrs: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> Vec<String> {
        let mut key: Vec<String> = attrs
            .map(|kv| format!("{}={}", kv.key.as_str(), kv.value.as_str()))
            .collect();
        key.sort();
        key
    }
    match metric.data() {
        AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
            .data_points()
            .map(|dp| (key(dp.attributes()), dp.value()))
            .collect(),
        AggregatedMetrics::F64(MetricData::Histogram(hist)) => hist
            .data_points()
            .map(|dp| (key(dp.attributes()), dp.count()))
            .collect(),
        _ => panic!("{} has an unexpected shape", metric.name()),
    }
}

/// `entries` as a sorted point key.
fn key(entries: &[(&str, &str)]) -> Vec<String> {
    let mut key: Vec<String> = entries.iter().map(|(k, v)| format!("{k}={v}")).collect();
    key.sort();
    key
}

const OUTCOME: &str = semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME;
const TRIGGER: &str = semconv::OURIOS_TEMPLATE_MAP_ACQUIRE_TRIGGER;
const REASON: &str = semconv::OURIOS_TEMPLATE_MAP_DERIVE_REASON;
const MODE: &str = semconv::OURIOS_TEMPLATE_MAP_DERIVE_MODE;

/// The points `added` holds for `metric`.
fn of(added: &Points, metric: &str) -> BTreeMap<Vec<String>, u64> {
    added.get(metric).cloned().unwrap_or_default()
}

fn one(entries: &[(&str, &str)]) -> BTreeMap<Vec<String>, u64> {
    [(key(entries), 1)].into_iter().collect()
}

/// Rewrite `tenant`'s published artifact without its `fold_state`.
fn strip_fold_state(root: &Path, tenant: &str) {
    let path = root
        .join("audit")
        .join(format!("tenant_id={tenant}"))
        .join(TEMPLATE_MAP_FILENAME);
    let compressed = std::fs::read(&path).expect("read artifact");
    let json = zstd::decode_all(compressed.as_slice()).expect("decompress artifact");
    let mut value: serde_json::Value = serde_json::from_slice(&json).expect("artifact json");
    value.as_object_mut().expect("object").remove("fold_state");
    let json = serde_json::to_vec(&value).expect("serialize artifact");
    std::fs::write(
        path,
        zstd::encode_all(json.as_slice(), 3).expect("compress"),
    )
    .expect("write artifact");
}

/// Scenario RFC0060.1: the pass pays the `no_fold_state` full fold under
/// `trigger = refresh`; the query after it is a `hit` under `query`.
async fn bootstrap_act(pipeline: &Pipeline) {
    let dir = store(&["acme"]);
    let querier = Querier::new(dir.path());
    assert_eq!(query(&querier, "acme").await, 1, "the cold query publishes");
    strip_fold_state(dir.path(), "acme");
    write_audit(dir.path(), &[adopted("acme", 2, TS0 + HOUR_NS)]);

    let mut refresher = querier.template_map_refresher();
    let added = pipeline.added(refresher.run_pass()).await;
    assert_eq!(
        of(&added, semconv::OURIOS_TEMPLATE_MAP_LOOKUPS),
        one(&[(OUTCOME, "stale"), (TRIGGER, "refresh")])
    );
    assert_eq!(
        of(&added, semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION),
        one(&[
            (
                semconv::OURIOS_TEMPLATE_MAP_DERIVE_FALLBACK_REASON,
                "no_fold_state"
            ),
            (MODE, "full"),
            (REASON, "stale"),
            (TRIGGER, "refresh"),
        ]),
        "the full fold ran with trigger = refresh"
    );

    let added = pipeline
        .added(async {
            assert_eq!(query(&querier, "acme").await, 1);
        })
        .await;
    assert_eq!(
        of(&added, semconv::OURIOS_TEMPLATE_MAP_LOOKUPS),
        one(&[(OUTCOME, "hit"), (TRIGGER, "query")])
    );
    assert!(
        of(&added, semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION).is_empty(),
        "the query runs no fold"
    );
}

/// Scenario RFC0060.2: the query that joins the task's fold records
/// `joined` with the task's trigger.
async fn join_act(pipeline: &Pipeline) {
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
    let added = pipeline
        .added(async {
            let mut refresher = querier.template_map_refresher();
            let pass = tokio::spawn(async move { refresher.run_pass().await });
            let deadline = Instant::now() + Duration::from_secs(30);
            while querier.template_map_acquisitions().is_empty() {
                assert!(Instant::now() < deadline, "the task never acquired");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            assert_eq!(query(&querier, "acme").await, 1);
            pass.await.expect("pass task");
        })
        .await;
    let expected: BTreeMap<Vec<String>, u64> = [
        (key(&[(OUTCOME, "miss"), (TRIGGER, "refresh")]), 1),
        (key(&[(OUTCOME, "joined"), (TRIGGER, "refresh")]), 1),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        of(&added, semconv::OURIOS_TEMPLATE_MAP_LOOKUPS),
        expected,
        "the joined lookup names who started the acquisition"
    );
    assert_eq!(
        of(&added, semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION),
        one(&[(MODE, "full"), (REASON, "miss"), (TRIGGER, "refresh")]),
        "exactly one fold"
    );
}

/// Scenario RFC0060.4: a pass that handled a failed tenant records a plain
/// sample; a pass whose listing failed records `error.type`.
async fn failure_act(pipeline: &Pipeline) {
    let dir = store(&["a", "b"]);
    let audit = dir.path().join("audit").join("tenant_id=a");
    let mut dirs = vec![audit];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                std::fs::write(&path, b"not parquet").expect("corrupt");
            }
        }
    }
    let querier = Querier::new(dir.path());
    let mut refresher = querier.template_map_refresher();
    let added = pipeline.added(refresher.run_pass()).await;
    assert_eq!(
        of(&added, semconv::OURIOS_TEMPLATE_MAP_REFRESH_DURATION),
        one(&[]),
        "one sample, no error.type for a tenant the pass handled"
    );

    std::fs::rename(dir.path().join("audit"), dir.path().join("moved")).expect("move audit/");
    std::fs::write(dir.path().join("audit"), b"not a directory").expect("plant a file");
    let added = pipeline.added(refresher.run_pass()).await;
    assert_eq!(
        of(&added, semconv::OURIOS_TEMPLATE_MAP_REFRESH_DURATION),
        one(&[("error.type", "storage")]),
        "a failed listing ends the pass with error.type"
    );
}

/// Scenarios RFC0060.1, .2, .4 (metrics) and .8 (instrument shape).
#[test]
fn rfc0060_refresh_metrics() {
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let pipeline = Pipeline { guard, exporter };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        bootstrap_act(&pipeline).await;
        join_act(&pipeline).await;
        failure_act(&pipeline).await;
    });

    let rms = pipeline.collection();
    let refresh = metric(&rms, semconv::OURIOS_TEMPLATE_MAP_REFRESH_DURATION)
        .expect("refresh.duration exported");
    assert_eq!(refresh.unit(), "s");
    ourios_telemetry::histogram_guard::assert_explicit_buckets(
        std::slice::from_ref(&rms),
        &[semconv::OURIOS_TEMPLATE_MAP_REFRESH_DURATION],
    );
}
