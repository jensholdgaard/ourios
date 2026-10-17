//! RFC0033.7 — observable outcomes.
//!
//! With the `OTel` metrics pipeline active (the RFC 0016 in-memory
//! metrics-pipeline test shape, RFC 0033 §6), queries through the public
//! query surface drive a miss, a hit, a staleness, and a torn artifact;
//! the §3.7 lookup-outcome and publish-outcome instruments record each
//! with the correct outcome attribute, and the publish-size instrument
//! records the artifact size. Instrument and attribute names come from
//! the weaver-generated `ourios-semconv` constants (the registry half of
//! the scenario is the CI semconv no-diff gate).
//!
//! With the `testing` feature (CI runs `--all-features`), two concurrent
//! same-frontier queries on one querier also drive a shared acquisition:
//! the acquirer records its own outcome and the waiter records `joined`.
//!
//! The same acts pin the derive-cost instruments (RFC0033.9, #924): one
//! `derive.duration` sample per fold keyed by `derive.reason` (none for a
//! hit, `error.type` on a failed fold), audit files and size per
//! completed fold — the cold one exactly the fixture's audit set — and an
//! `acquire.wait.duration` sample per joiner, whether the shared
//! acquisition succeeded or failed. A last act covers `unknown_version`
//! and a failure before classification, which records no derive sample.
//!
//! This test installs a process-global in-memory `MeterProvider`, so it
//! lives in its own integration binary (its own process) — the
//! `rfc0016_6_query_metrics` precedent, applied at the querier-library
//! surface where the RFC 0033 instruments live — and is one of this
//! crate's RFC0028.2 process-isolation exemptions (with
//! `rfc0060_refresh_observability.rs`, the same shape, and
//! `rfc0033_bounded_fold.rs`, which installs a global allocator).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
};
use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, ParamType, TemplateChange, hash_triggering_line,
};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{ParquetAuditSink, PartitionKey, Store, Writer};
use ourios_querier::{Querier, QueryResult, TEMPLATE_MAP_FILENAME};
use ourios_semconv as semconv;
use ourios_telemetry::live_check::{self, Checked, EventSpec};

const TENANT: &str = "acme";
/// 2026-04-02T10:58:00 UTC — the shared fixture instant of the querier
/// integration suite, comfortably in the past.
const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;
const NOW: u64 = TS0 + 24 * HOUR_NS;
const DEFAULT_WINDOW_NS: u64 = 30 * 24 * HOUR_NS;

/// A minimal mined record for `template_id` at `ts_ns` — enough for a
/// body-rendering query to match and render.
fn mined(template_id: u64, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(TENANT),
        template_id,
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

/// A `template_widened` audit event for `template_id` at `ts_ns`,
/// bumping `old_version` → `old_version + 1`.
fn widened(template_id: u64, old_version: u32, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(TENANT),
        timestamp: UNIX_EPOCH + Duration::from_nanos(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Widened {
                old_version,
                new_version: old_version + 1,
                old_template: "user <*>".to_string(),
                new_template: "user <*> <*>".to_string(),
                positions_widened: vec![1],
            },
        },
    }
}

/// A `template_created` audit event binding `template_id` v1 to
/// `template` at `ts_ns`.
fn created(template_id: u64, template: &str, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(TENANT),
        timestamp: UNIX_EPOCH + Duration::from_nanos(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: template.to_string(),
            },
        },
    }
}

/// Seed audit events through the production `ParquetAuditSink` (real
/// audit Parquet, the same stream the fold reads).
fn write_audit(bucket: &Path, events: &[AuditEvent]) {
    let mut sink = ParquetAuditSink::new(Store::local(bucket).expect("store"));
    for e in events {
        sink.emit(e.clone());
    }
    assert_eq!(sink.write_failures(), 0, "audit fixtures must all persist");
}

/// A body-rendering query (`template_id == 1 | limit 10` — the `limit`
/// makes it materialise rows, RFC 0017) through the public query
/// surface; the template-map acquisition runs inside row rendering.
fn body_query(bucket: &Path) -> QueryResult {
    try_body_query(bucket).expect("every §3.3 disposition answers the query, no error surfaced")
}

fn try_body_query(bucket: &Path) -> Result<QueryResult, ourios_querier::QueryError> {
    let query = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
    let querier = Querier::new(bucket);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(querier.run_query(
        &query,
        &TenantId::new(TENANT),
        NOW,
        DEFAULT_WINDOW_NS,
        None,
    ))
}

/// Two concurrent body-rendering queries on one querier at an unchanged
/// frontier. The first to list acquires; a gate holds that acquisition
/// until the other has found it in flight and parked on it, so the second
/// deterministically joins (one lookup outcome each: the acquirer's own,
/// and `joined`).
#[cfg(feature = "testing")]
fn shared_acquisition(bucket: &Path) {
    let rows: Vec<u64> = run_shared(bucket)
        .into_iter()
        .map(|r| r.expect("shared acquisition answers both queries"))
        .collect();
    assert_eq!(rows, vec![1, 1]);
}

/// Act 5b: the shared acquisition at act 5's unreadable frontier. The
/// acquirer's fold fails, and the waiter gets the same error — and still
/// records how long it waited (RFC0033.9), though no lookup outcome.
#[cfg(feature = "testing")]
fn shared_failing_acquisition(bucket: &Path) {
    let results = run_shared(bucket);
    assert!(
        results.iter().all(Result::is_err),
        "a failed shared acquisition fails both queries: {results:?}",
    );
}

/// Run two concurrent body-rendering queries on one querier with the
/// acquirer gated until the other has joined; each query's row count.
#[cfg(feature = "testing")]
fn run_shared(bucket: &Path) -> Vec<Result<u64, ourios_querier::QueryError>> {
    use std::sync::Arc;
    use std::time::Instant;

    let querier = Arc::new(Querier::new(bucket));
    let weak = Arc::downgrade(&querier);
    querier.set_template_map_acquire_gate(Some(Arc::new(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(querier) = weak.upgrade()
            && querier.template_map_waits() < 1
        {
            assert!(Instant::now() < deadline, "the second query never joined");
            std::thread::sleep(Duration::from_millis(2));
        }
    })));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("runtime");
    let results = runtime.block_on(async {
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let querier = Arc::clone(&querier);
                tokio::spawn(async move {
                    let query =
                        ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
                    querier
                        .run_query(&query, &TenantId::new(TENANT), NOW, DEFAULT_WINDOW_NS, None)
                        .await
                        .map(|result| result.rows)
                })
            })
            .collect();
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await.expect("query task"));
        }
        results
    });
    querier.set_template_map_acquire_gate(None);
    assert_eq!(querier.template_map_waits(), 1, "exactly one query waited");
    results
}

/// The lookup outcomes the acts below drive, once each — plus, with the
/// `testing` feature, the shared acquisition's acquirer (a second `hit`)
/// and its waiter (`joined`).
#[cfg(not(feature = "testing"))]
const EXPECTED_LOOKUPS: &[(&str, u64)] = &[("miss", 1), ("hit", 1), ("stale", 1), ("torn", 1)];
#[cfg(feature = "testing")]
const EXPECTED_LOOKUPS: &[(&str, u64)] = &[
    ("miss", 1),
    ("hit", 2),
    ("stale", 1),
    ("torn", 1),
    ("joined", 1),
];

/// Append one new audit file under `audit_dir` and tear it in place, so
/// the frontier changes and the fold over it fails.
fn append_torn_audit_file(bucket: &Path, audit_dir: &Path) {
    let before = walk_files(audit_dir);
    write_audit(bucket, &[widened(1, 4, TS0 + 2 * HOUR_NS)]);
    let new_audit = walk_files(audit_dir)
        .into_iter()
        .find(|p| !before.contains(p))
        .expect("act 5 wrote a new audit file");
    let bytes = std::fs::read(&new_audit).expect("read new audit file");
    std::fs::write(&new_audit, &bytes[..bytes.len() / 2]).expect("tear new audit file");
}

/// Every file under `dir`, recursively — the audit stream is
/// date-partitioned, so new files land in nested directories.
fn walk_files(dir: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
    let mut files = std::collections::BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).expect("list audit dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path);
            }
        }
    }
    files
}

fn metric_data<'a>(rms: &'a [ResourceMetrics], name: &str) -> &'a AggregatedMetrics {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("metric {name} missing from the exported stream"))
        .data()
}

/// The `ourios.template_map.acquire.trigger` every lookup and derive sample
/// in this file carries (RFC 0060 §3.5): each acquisition here is a query's.
const QUERY_TRIGGER: (&str, &str) = (semconv::OURIOS_TEMPLATE_MAP_ACQUIRE_TRIGGER, "query");

/// The counter `metric`'s data points as `attr_key value → count` — one
/// entry per exported attribute set, so extras (a stray outcome, an
/// attribute-free point) fail the map equality below. Every point must also
/// carry exactly the `fixed` attributes, and nothing else.
fn outcome_counts(
    rms: &[ResourceMetrics],
    metric: &str,
    attr_key: &str,
    fixed: &[(&str, &str)],
) -> BTreeMap<String, u64> {
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric_data(rms, metric) else {
        panic!("{metric} should be a u64 sum (counter)");
    };
    let mut counts = BTreeMap::new();
    for dp in sum.data_points() {
        let (fixed_attrs, attrs): (Vec<_>, Vec<_>) = dp
            .attributes()
            .partition(|kv| fixed.iter().any(|(key, _)| kv.key.as_str() == *key));
        let mut carried: Vec<(String, String)> = fixed_attrs
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
            .collect();
        carried.sort();
        let mut expected: Vec<(String, String)> = fixed
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        expected.sort();
        assert_eq!(carried, expected, "{metric} data point's fixed attributes");
        let [kv] = attrs.as_slice() else {
            panic!(
                "{metric} data point must carry exactly the {attr_key} attribute, \
                 got {} attributes",
                attrs.len(),
            );
        };
        assert_eq!(
            kv.key.as_str(),
            attr_key,
            "{metric} data point carries an unexpected attribute",
        );
        let outcome = kv.value.as_str().into_owned();
        assert!(
            counts.insert(outcome.clone(), dp.value()).is_none(),
            "{metric} exported two series for {attr_key}={outcome}",
        );
    }
    counts
}

/// A histogram's data points as `sorted "key=value" attributes → (count,
/// sum)`, for either number type.
fn histogram_points(rms: &[ResourceMetrics], metric: &str) -> BTreeMap<Vec<String>, (u64, f64)> {
    fn key<'a>(attrs: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> Vec<String> {
        let mut key: Vec<String> = attrs
            .map(|kv| format!("{}={}", kv.key.as_str(), kv.value.as_str()))
            .collect();
        key.sort();
        key
    }
    let mut points = BTreeMap::new();
    match metric_data(rms, metric) {
        AggregatedMetrics::F64(MetricData::Histogram(hist)) => {
            for dp in hist.data_points() {
                points.insert(key(dp.attributes()), (dp.count(), dp.sum()));
            }
        }
        AggregatedMetrics::U64(MetricData::Histogram(hist)) => {
            for dp in hist.data_points() {
                #[allow(clippy::cast_precision_loss)] // fixture-sized sums
                points.insert(key(dp.attributes()), (dp.count(), dp.sum() as f64));
            }
        }
        _ => panic!("{metric} should be a histogram"),
    }
    points
}

/// The tenant's audit `*.parquet` files and their total bytes: what a
/// derive over the whole audit set reads.
fn audit_set(audit_dir: &Path) -> (u64, u64) {
    walk_files(audit_dir)
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "parquet"))
        .fold((0, 0), |(n, bytes), p| {
            (
                n + 1,
                bytes + std::fs::metadata(p).expect("stat audit file").len(),
            )
        })
}

/// The `ourios.template_map.derive.reason` attribute as a
/// [`histogram_points`] key entry.
fn reason(r: &str) -> String {
    format!("{}={r}", semconv::OURIOS_TEMPLATE_MAP_DERIVE_REASON)
}

/// The `ourios.template_map.derive.mode` attribute as a key entry.
fn mode(m: &str) -> String {
    format!("{}={m}", semconv::OURIOS_TEMPLATE_MAP_DERIVE_MODE)
}

/// The `ourios.template_map.derive.fallback.reason` attribute as a key
/// entry.
fn fallback(r: &str) -> String {
    format!(
        "{}={r}",
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_FALLBACK_REASON
    )
}

/// A derive [`histogram_points`] key: `entries` and the query trigger,
/// sorted.
fn key(entries: &[String]) -> Vec<String> {
    let (trigger, value) = QUERY_TRIGGER;
    let mut key = entries.to_vec();
    key.push(format!("{trigger}={value}"));
    key.sort();
    key
}

/// The derive key of a completed full fold for `derive_reason`.
fn full(derive_reason: &str) -> Vec<String> {
    key(&[mode("full"), reason(derive_reason)])
}

/// A histogram's sample count per attribute set.
fn sample_counts(rms: &[ResourceMetrics], metric: &str) -> BTreeMap<Vec<String>, u64> {
    histogram_points(rms, metric)
        .into_iter()
        .map(|(k, (count, _))| (k, count))
        .collect()
}

/// Failed `stale` folds: act 5's, plus act 5b's acquirer with `testing`.
#[cfg(not(feature = "testing"))]
const FAILED_STALE_FOLDS: u64 = 1;
#[cfg(feature = "testing")]
const FAILED_STALE_FOLDS: u64 = 2;

/// Scenario RFC0033.9 (#924) — derive cost is observable.
/// See `docs/rfcs/0033-cached-template-map.md` §5.
///
/// Acts 1–5 fold `miss`, `stale` and `torn`, then fail a `stale` fold
/// (act 5, and act 5b's acquirer with `testing`), which records its
/// duration with `error.type` and no audit files or size. The hits (act
/// 2, and act 4b's acquirer) never derive. The `unknown_version` reason
/// is [`unknown_version_act`]'s.
///
/// Scenario RFC0033.13: act 3's `stale` derive extends the artifact
/// (`mode = incremental`); the miss and torn derives are `full`. Act 5's
/// stale derive fails reading its delta before the guard decides, so its
/// sample carries `error.type` and no `mode`.
fn assert_derive_metrics(rms: &[ResourceMetrics], cold_audit_set: (u64, u64)) {
    let incremental = key(&[mode("incremental"), reason("stale")]);
    let expected: BTreeMap<Vec<String>, u64> = [
        (full("miss"), 1),
        (incremental.clone(), 1),
        (full("torn"), 1),
        (
            key(&["error.type=storage".to_string(), reason("stale")]),
            FAILED_STALE_FOLDS,
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        sample_counts(rms, semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION),
        expected,
        "one duration sample per fold and none for a hit; a failed fold carries error.type",
    );
    let completed: BTreeMap<Vec<String>, u64> =
        [(full("miss"), 1), (incremental, 1), (full("torn"), 1)]
            .into_iter()
            .collect();
    for metric in [
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_FILES,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_SIZE,
    ] {
        assert_eq!(
            sample_counts(rms, metric),
            completed,
            "{metric}: one sample per completed fold"
        );
    }
    assert_audit_read(rms, &full("miss"), cold_audit_set);
    assert_acquire_waits(rms);
}

/// The derive `key`'s single files and size samples equal `audit_set`,
/// the fixture's audit file count and bytes.
fn assert_audit_read(rms: &[ResourceMetrics], key: &[String], audit_set: (u64, u64)) {
    let (files, bytes) = audit_set;
    assert!(files > 0 && bytes > 0, "the fixture has an audit set");
    let key = key.to_vec();
    #[allow(clippy::cast_precision_loss)] // fixture-sized values
    let expected = [(1, files as f64), (1, bytes as f64)];
    let observed = [
        histogram_points(rms, semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_FILES)[&key],
        histogram_points(rms, semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_SIZE)[&key],
    ];
    assert_eq!(
        observed, expected,
        "the {key:?} derive read exactly the fixture's audit files and bytes",
    );
}

/// Joiners alone record waits: act 4b's after a successful shared
/// acquisition, act 5b's after a failed one. Neither acquirer does.
#[cfg(feature = "testing")]
fn assert_acquire_waits(rms: &[ResourceMetrics]) {
    let expected: BTreeMap<Vec<String>, u64> = [(Vec::new(), 2)].into_iter().collect();
    assert_eq!(
        sample_counts(rms, semconv::OURIOS_TEMPLATE_MAP_ACQUIRE_WAIT_DURATION),
        expected,
        "each joiner records one attribute-free wait sample; the acquirers none",
    );
}

#[cfg(not(feature = "testing"))]
fn assert_acquire_waits(rms: &[ResourceMetrics]) {
    let names: Vec<_> = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .map(opentelemetry_sdk::metrics::data::Metric::name)
        .collect();
    assert!(
        !names.contains(&semconv::OURIOS_TEMPLATE_MAP_ACQUIRE_WAIT_DURATION),
        "no query joined, so no wait was recorded",
    );
}

/// Rewrite the published artifact at `path` in place, editing its JSON
/// body.
fn rewrite_artifact(path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let compressed = std::fs::read(path).expect("read artifact");
    let json = zstd::decode_all(compressed.as_slice()).expect("decompress artifact");
    let mut value: serde_json::Value = serde_json::from_slice(&json).expect("artifact json");
    edit(&mut value);
    let json = serde_json::to_vec(&value).expect("serialize artifact");
    let compressed = zstd::encode_all(json.as_slice(), 3).expect("compress artifact");
    std::fs::write(path, compressed).expect("write artifact");
}

/// Scenario RFC0033.9 (#924), the `unknown_version` reason and a failure
/// before classification, on a store of their own. Run last: the
/// cumulative collection still holds the earlier acts' series, but none
/// of them carries `unknown_version`.
///
/// A future writer's artifact folds once under `unknown_version`, reading
/// the whole audit set. An artifact naming another tenant fails the query
/// before any fold runs, so it records no derive sample.
fn unknown_version_act(
    guard: &ourios_telemetry::TelemetryGuard,
    exporter: &opentelemetry_sdk::metrics::InMemoryMetricExporter,
) {
    let bucket = tempfile::tempdir().expect("temp dir");
    write_audit(bucket.path(), &[widened(1, 1, TS0), widened(1, 2, TS0 + 1)]);
    write_records(bucket.path(), &[mined(1, TS0 + 2)]);
    let audit_dir = bucket
        .path()
        .join("audit")
        .join(format!("tenant_id={TENANT}"));
    let artifact = audit_dir.join(TEMPLATE_MAP_FILENAME);
    assert_eq!(body_query(bucket.path()).rows, 1, "the miss publishes");
    rewrite_artifact(&artifact, |json| json["format_version"] = 3.into());
    assert_eq!(body_query(bucket.path()).rows, 1);

    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let rms = std::slice::from_ref(rms.last().expect("a collection"));
    assert_eq!(
        sample_counts(rms, semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION)
            .get(&full("unknown_version")),
        Some(&1),
        "one duration sample for the unknown-version fold",
    );
    assert_audit_read(rms, &full("unknown_version"), audit_set(&audit_dir));
    let folds = |rms: &[ResourceMetrics]| -> u64 {
        sample_counts(rms, semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION)
            .values()
            .sum()
    };
    let before = folds(rms);

    rewrite_artifact(&artifact, |json| json["tenant_id"] = "intruder".into());
    try_body_query(bucket.path()).expect_err("an artifact naming another tenant fails the query");
    guard.force_flush().expect("force_flush");
    let after = exporter.get_finished_metrics().expect("metrics exported");
    let after = std::slice::from_ref(after.last().expect("a collection"));
    assert_eq!(folds(after), before, "no fold ran, so no derive sample");
}

const BINDING_CONFLICTED: EventSpec = EventSpec {
    name: semconv::EVENT_OURIOS_TEMPLATE_MAP_BINDING_CONFLICTED,
    required: &[
        semconv::OURIOS_TENANT,
        semconv::OURIOS_TEMPLATE_ID,
        semconv::OURIOS_TEMPLATE_VERSION,
    ],
    optional: &[],
};

/// The lookups counter's data points that carry the anomaly attribute,
/// as `(outcome, anomaly) → count`.
fn anomaly_counts(rms: &[ResourceMetrics]) -> BTreeMap<(String, String), u64> {
    let AggregatedMetrics::U64(MetricData::Sum(sum)) =
        metric_data(rms, semconv::OURIOS_TEMPLATE_MAP_LOOKUPS)
    else {
        panic!("lookups should be a u64 sum (counter)");
    };
    let mut counts = BTreeMap::new();
    for dp in sum.data_points() {
        let attr = |key: &str| {
            dp.attributes()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.as_str().into_owned())
        };
        if let Some(anomaly) = attr(semconv::OURIOS_TEMPLATE_MAP_LOOKUP_ANOMALY) {
            let outcome = attr(semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME)
                .expect("an anomalous lookup still carries its outcome");
            counts.insert((outcome, anomaly), dp.value());
        }
    }
    counts
}

/// Scenario RFC0033.8 (#927) — a store whose audit stream binds two
/// different templates to one `(template_id, version)` reports it once
/// per fold-backed acquisition: one named event carrying the key and no
/// template text, and the anomaly on that lookup. A same-binding re-emit and a widening
/// stay silent, and a cache hit (no fold) reports nothing.
///
/// Act 6 of RFC0033.7, run last: its lookups share the process-global
/// counter, so it first checks `conflict_free` (the collection from acts
/// 1–5) carries no anomaly, then resets the exporter.
fn binding_conflict_act(
    guard: &ourios_telemetry::TelemetryGuard,
    exporter: &opentelemetry_sdk::metrics::InMemoryMetricExporter,
    conflict_free: &[ResourceMetrics],
) {
    assert!(
        anomaly_counts(conflict_free).is_empty(),
        "a conflict-free store records no lookup anomaly",
    );
    exporter.reset();
    let capture = live_check::event_capture().expect("the only subscriber this binary installs");
    capture.reset();
    let bucket = tempfile::tempdir().expect("temp dir");
    write_audit(
        bucket.path(),
        &[
            created(1, "alpha <*>", TS0),
            created(1, "omega <*>", TS0 + 1),
            created(2, "same <*>", TS0),
            created(2, "same <*>", TS0 + 2),
            widened(2, 1, TS0 + 3),
        ],
    );
    write_records(bucket.path(), &[mined(1, TS0 + 4)]);

    assert_eq!(
        body_query(bucket.path()).rows,
        1,
        "a conflict never fails the query"
    );
    assert_eq!(body_query(bucket.path()).rows, 1);

    let events: Vec<_> = capture
        .events()
        .into_iter()
        .filter(|e| e.name == BINDING_CONFLICTED.name)
        .collect();
    let [event] = events.as_slice() else {
        panic!("one conflict, reported once by the folding acquisition: {events:#?}");
    };
    let attr = |key: &str| event.attributes.get(key).map(String::as_str);
    assert_eq!(attr(semconv::OURIOS_TENANT), Some(TENANT));
    assert_eq!(attr(semconv::OURIOS_TEMPLATE_ID), Some("1"));
    assert_eq!(attr(semconv::OURIOS_TEMPLATE_VERSION), Some("1"));
    let body = event.body.as_deref().unwrap_or_default();
    for text in ["alpha", "omega"] {
        assert!(
            !body.contains(text) && event.attributes.values().all(|v| !v.contains(text)),
            "the event must not carry template text: {event:#?}",
        );
    }
    let checked = live_check::live_check(&events, &[BINDING_CONFLICTED])
        .expect("the conflict event is registry-conformant");
    if checked == Checked::SpecOnly {
        eprintln!("#927: weaver is not configured here; checked the event against its spec only");
    }

    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let expected: BTreeMap<(String, String), u64> =
        [(("miss".to_string(), "binding_conflict".to_string()), 1)]
            .into_iter()
            .collect();
    assert_eq!(
        anomaly_counts(&rms),
        expected,
        "the folding miss carries the anomaly; the hit after it does not",
    );

    announced_once_act(guard, exporter, capture, bucket.path());
}

/// Scenario RFC0033.8, amendment 2026-10-13 (#987): a refresh of the
/// artifact the conflicting miss published re-announces nothing for the
/// known key, though its lookup still carries the anomaly; a second key
/// coming into conflict is announced once, alone; and a `files_removed`
/// full-fold fallback re-announces neither known key.
fn announced_once_act(
    guard: &ourios_telemetry::TelemetryGuard,
    exporter: &opentelemetry_sdk::metrics::InMemoryMetricExporter,
    capture: &live_check::EventCapture,
    bucket: &Path,
) {
    let conflict_events = || conflict_events(capture);
    capture.reset();

    let before_fresh = audit_parquet_files(bucket);
    write_audit(bucket, &[created(5, "fresh <*>", TS0 + 5)]);
    let fresh: Vec<_> = audit_parquet_files(bucket)
        .difference(&before_fresh)
        .cloned()
        .collect();
    assert_eq!(body_query(bucket).rows, 1);
    let events = conflict_events();
    assert!(
        events.is_empty(),
        "a refresh re-announces no known key: {events:#?}"
    );

    write_audit(
        bucket,
        &[
            created(3, "left <*>", TS0 + 6),
            created(3, "right <*>", TS0 + 7),
        ],
    );
    assert_eq!(body_query(bucket).rows, 1);
    let events = conflict_events();
    let [event] = events.as_slice() else {
        panic!("only the new key is announced: {events:#?}");
    };
    assert_eq!(
        event
            .attributes
            .get(semconv::OURIOS_TEMPLATE_ID)
            .map(String::as_str),
        Some("3")
    );

    capture.reset();
    assert!(!fresh.is_empty(), "the fresh audit file was written");
    for file in &fresh {
        std::fs::remove_file(file).expect("remove a folded audit file");
    }
    assert_eq!(body_query(bucket).rows, 1);
    let events = conflict_events();
    assert!(
        events.is_empty(),
        "a files_removed full fold re-announces no known key: {events:#?}"
    );

    assert_refresh_anomalies(guard, exporter);
}

/// The binding-conflict events captured since the last reset.
fn conflict_events(capture: &live_check::EventCapture) -> Vec<live_check::Event> {
    capture
        .events()
        .into_iter()
        .filter(|e| e.name == BINDING_CONFLICTED.name)
        .collect()
}

/// The audit Parquet objects under `bucket`.
fn audit_parquet_files(bucket: &Path) -> std::collections::BTreeSet<PathBuf> {
    walk_files(&bucket.join("audit"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "parquet"))
        .collect()
}

/// `announced_once_act`'s lookups: the conflicting miss and its three
/// stale refreshes each carry the anomaly.
fn assert_refresh_anomalies(
    guard: &ourios_telemetry::TelemetryGuard,
    exporter: &opentelemetry_sdk::metrics::InMemoryMetricExporter,
) {
    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let rms = std::slice::from_ref(rms.last().expect("a collection"));
    let expected: BTreeMap<(String, String), u64> = [
        (("miss".to_string(), "binding_conflict".to_string()), 1),
        (("stale".to_string(), "binding_conflict".to_string()), 3),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        anomaly_counts(rms),
        expected,
        "every refresh of a conflicted map still carries the anomaly",
    );
}

/// The publish-size histogram recorded each published artifact's byte
/// size (the number RFC0033.6 gates on): one sample per publish.
fn assert_artifact_sizes(rms: &[ResourceMetrics], published: [u64; 3]) {
    let AggregatedMetrics::U64(MetricData::Histogram(hist)) =
        metric_data(rms, semconv::OURIOS_TEMPLATE_MAP_ARTIFACT_SIZE)
    else {
        panic!("artifact.size should be a u64 histogram");
    };
    let (count, sum) = hist
        .data_points()
        .map(|dp| (dp.count(), dp.sum()))
        .fold((0, 0), |(c, s), (dc, ds)| (c + dc, s + ds));
    assert_eq!(count, 3, "one size sample per publish");
    assert_eq!(
        sum,
        published.iter().sum::<u64>(),
        "each sample is the published artifact's byte size",
    );
}

/// RFC0033.7's counted outcomes: one lookup per answered outcome, each
/// `query`-triggered, and one publish per write-through that published.
fn assert_lookups_and_publishes(rms: &[ResourceMetrics]) {
    let lookups = outcome_counts(
        rms,
        semconv::OURIOS_TEMPLATE_MAP_LOOKUPS,
        semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME,
        &[QUERY_TRIGGER],
    );
    let expected: BTreeMap<String, u64> = EXPECTED_LOOKUPS
        .iter()
        .map(|(outcome, n)| ((*outcome).to_string(), *n))
        .collect();
    assert_eq!(
        lookups, expected,
        "one lookup per answered outcome, no extras — act 5's failed fold \
         counted nothing"
    );

    // The three publishing lookups (miss, stale, torn) each published;
    // no lost race or error occurred, so no other series exists.
    let publishes = outcome_counts(
        rms,
        semconv::OURIOS_TEMPLATE_MAP_PUBLISHES,
        semconv::OURIOS_TEMPLATE_MAP_PUBLISH_OUTCOME,
        &[],
    );
    let expected: BTreeMap<String, u64> = [("published".to_string(), 3)].into_iter().collect();
    assert_eq!(publishes, expected, "three write-through publishes");
}

/// The template-map instruments the acts drive, each exported with the
/// registry's description and unit.
const REGISTERED: &[ourios_telemetry::metadata_guard::Registered] = &[
    (
        semconv::OURIOS_TEMPLATE_MAP_LOOKUPS,
        semconv::OURIOS_TEMPLATE_MAP_LOOKUPS_DESCRIPTION,
        semconv::OURIOS_TEMPLATE_MAP_LOOKUPS_UNIT,
    ),
    (
        semconv::OURIOS_TEMPLATE_MAP_PUBLISHES,
        semconv::OURIOS_TEMPLATE_MAP_PUBLISHES_DESCRIPTION,
        semconv::OURIOS_TEMPLATE_MAP_PUBLISHES_UNIT,
    ),
    (
        semconv::OURIOS_TEMPLATE_MAP_ARTIFACT_SIZE,
        semconv::OURIOS_TEMPLATE_MAP_ARTIFACT_SIZE_DESCRIPTION,
        semconv::OURIOS_TEMPLATE_MAP_ARTIFACT_SIZE_UNIT,
    ),
    (
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION_DESCRIPTION,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION_UNIT,
    ),
    (
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_FILES,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_FILES_DESCRIPTION,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_FILES_UNIT,
    ),
    (
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_SIZE,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_SIZE_DESCRIPTION,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_SIZE_UNIT,
    ),
];

/// Scenario RFC0033.7 — observable outcomes.
/// Scenario RFC0033.9 — derive cost is observable (`assert_derive_metrics`).
/// See `docs/rfcs/0033-cached-template-map.md` §5.
#[test]
fn rfc0033_7_observable_outcomes() {
    // Arrange — install the in-memory global meter FIRST, so the lazily
    // built instruments resolve against it; then a store with an audit
    // history and one matching data row.
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let bucket = tempfile::tempdir().expect("temp dir");
    write_audit(bucket.path(), &[widened(1, 1, TS0), widened(1, 2, TS0 + 1)]);
    write_records(bucket.path(), &[mined(1, TS0 + 2)]);
    let artifact = bucket
        .path()
        .join("audit")
        .join(format!("tenant_id={TENANT}"))
        .join(TEMPLATE_MAP_FILENAME);
    let artifact_len = || std::fs::metadata(&artifact).expect("stat artifact").len();
    let cold_audit_set = audit_set(artifact.parent().expect("audit tenant dir"));

    // Act — queries drive the four §5.7 outcomes in turn. Every answer
    // is identical (the cache is advisory); only the telemetry differs.
    // 1. No artifact: a miss, and the write-through publishes.
    assert_eq!(body_query(bucket.path()).rows, 1);
    assert!(artifact.exists(), "the miss write-through published");
    let size_after_miss = artifact_len();
    // 2. Unchanged store: a hit (no publish).
    assert_eq!(body_query(bucket.path()).rows, 1);
    // 3. A new audit file appears: stale, republished at the new frontier.
    write_audit(bucket.path(), &[widened(1, 3, TS0 + HOUR_NS)]);
    assert_eq!(body_query(bucket.path()).rows, 1);
    let size_after_stale = artifact_len();
    // 4. The artifact torn in place: treated as absent, self-healed by
    //    the write-through.
    let good = std::fs::read(&artifact).expect("read artifact");
    std::fs::write(&artifact, &good[..good.len() / 2]).expect("tear artifact");
    assert_eq!(body_query(bucket.path()).rows, 1);
    let size_after_torn = artifact_len();
    // 4b. Two concurrent queries at that frontier share one acquisition (an
    //     artifact hit, no publish): the waiter records `joined`.
    #[cfg(feature = "testing")]
    shared_acquisition(bucket.path());
    // 5. A failing fold records nothing: a frontier-changing audit file
    //    that is itself unreadable errors the query, so no outcome is
    //    counted — a counted outcome is always one that answered.
    append_torn_audit_file(bucket.path(), artifact.parent().expect("audit tenant dir"));
    try_body_query(bucket.path()).expect_err("an unreadable audit stream must fail the query");
    // 5b. The same failure under a shared acquisition: its waiter fails too,
    //     and records its wait but no lookup outcome.
    #[cfg(feature = "testing")]
    shared_failing_acquisition(bucket.path());

    // Assert — the exported stream carries the §3.7 instruments, every
    // driven outcome recorded once under its registry attribute value.
    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");

    assert_lookups_and_publishes(&rms);
    assert_artifact_sizes(&rms, [size_after_miss, size_after_stale, size_after_torn]);
    assert_derive_metrics(&rms, cold_audit_set);
    // #949 — no template-map histogram uses the SDK's millisecond buckets.
    ourios_telemetry::histogram_guard::assert_explicit_buckets(
        &rms,
        &[
            semconv::OURIOS_TEMPLATE_MAP_ARTIFACT_SIZE,
            semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION,
            semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_FILES,
            semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_SIZE,
        ],
    );
    ourios_telemetry::metadata_guard::assert_registry_metadata(&rms, REGISTERED);
    binding_conflict_act(&guard, &exporter, &rms);
    unknown_version_act(&guard, &exporter);
    derive_mode_act(&guard, &exporter);
    failure_after_decision_act(&guard, &exporter);
}

/// The derive histograms' `(count, sum)` per attribute set, from the
/// latest cumulative collection.
fn derive_points(
    guard: &ourios_telemetry::TelemetryGuard,
    exporter: &opentelemetry_sdk::metrics::InMemoryMetricExporter,
) -> [BTreeMap<Vec<String>, (u64, f64)>; 3] {
    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let rms = std::slice::from_ref(rms.last().expect("a collection"));
    [
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_DURATION,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_FILES,
        semconv::OURIOS_TEMPLATE_MAP_DERIVE_AUDIT_SIZE,
    ]
    .map(|metric| histogram_points(rms, metric))
}

/// What `after` added over `before`, per attribute set: `(count, sum)`.
fn added(
    before: &BTreeMap<Vec<String>, (u64, f64)>,
    after: &BTreeMap<Vec<String>, (u64, f64)>,
) -> BTreeMap<Vec<String>, (u64, f64)> {
    after
        .iter()
        .filter_map(|(key, &(count, sum))| {
            let (c0, s0) = before.get(key).copied().unwrap_or((0, 0.0));
            (count > c0).then(|| (key.clone(), (count - c0, sum - s0)))
        })
        .collect()
}

/// Run one body query after `setup`, which writes the audit file(s) the
/// next acquisition sees; return the parquet files and bytes `setup`
/// added.
fn step(bucket: &Path, audit_dir: &Path, setup: impl FnOnce()) -> (u64, u64) {
    let (files0, bytes0) = audit_set(audit_dir);
    setup();
    let (files1, bytes1) = audit_set(audit_dir);
    assert_eq!(body_query(bucket).rows, 1);
    (files1.saturating_sub(files0), bytes1.saturating_sub(bytes0))
}

/// What [`drive_derive_paths`] read: each incremental delta's files and
/// bytes, the rejected delta's, and the full audit set the `out_of_order`
/// fallback then folded.
struct DrivenPaths {
    deltas: [(u64, u64); 2],
    rejected: (u64, u64),
    full_after_rejection: (u64, u64),
}

/// Drive, on `path`'s published artifact, an incremental derive, the four
/// fallbacks (`out_of_order`, `depth_cap`, `no_fold_state`,
/// `files_removed`) and a second incremental derive.
fn drive_derive_paths(path: &Path, audit_dir: &Path) -> DrivenPaths {
    let artifact = audit_dir.join(TEMPLATE_MAP_FILENAME);
    let first = step(path, audit_dir, || {
        write_audit(path, &[widened(1, 2, TS0 + 10)]);
    });
    let rejected = step(path, audit_dir, || {
        write_audit(path, &[widened(1, 3, TS0 + 5)]);
    });
    let full_after_rejection = audit_set(audit_dir);
    step(path, audit_dir, || {
        rewrite_artifact(&artifact, |json| {
            json["fold_state"]["incremental_depth"] = 256.into();
        });
        write_audit(path, &[widened(1, 4, TS0 + 20)]);
    });
    step(path, audit_dir, || {
        rewrite_artifact(&artifact, |json| {
            json.as_object_mut().expect("object").remove("fold_state");
        });
        write_audit(path, &[widened(1, 5, TS0 + 30)]);
    });
    let newest = walk_files(audit_dir);
    let second = step(path, audit_dir, || {
        write_audit(path, &[widened(1, 6, TS0 + 40)]);
    });
    step(path, audit_dir, || {
        let added = walk_files(audit_dir)
            .into_iter()
            .find(|p| !newest.contains(p))
            .expect("the last audit file");
        std::fs::remove_file(added).expect("remove a folded file");
    });
    DrivenPaths {
        deltas: [first, second],
        rejected,
        full_after_rejection,
    }
}

/// Scenario RFC0033.13 — the path taken is observable, on a store of its
/// own. An incremental fold and each of the four fallbacks record
/// `derive.reason = stale` with their `derive.mode`, the fallbacks also
/// their `derive.fallback.reason`; the incremental sample's files and
/// size are the delta's, the `out_of_order` sample's the rejected delta
/// read plus the full fold.
#[allow(clippy::cast_precision_loss)] // fixture-sized values
fn derive_mode_act(
    guard: &ourios_telemetry::TelemetryGuard,
    exporter: &opentelemetry_sdk::metrics::InMemoryMetricExporter,
) {
    let bucket = tempfile::tempdir().expect("temp dir");
    let path = bucket.path();
    write_audit(path, &[widened(1, 1, TS0)]);
    write_records(path, &[mined(1, TS0 + 2)]);
    let audit_dir = path.join("audit").join(format!("tenant_id={TENANT}"));
    assert_eq!(body_query(path).rows, 1, "the miss publishes");
    let before = derive_points(guard, exporter);
    let driven = drive_derive_paths(path, &audit_dir);
    let after = derive_points(guard, exporter);

    let incremental = key(&[mode("incremental"), reason("stale")]);
    let declined = |r: &str| key(&[fallback(r), mode("full"), reason("stale")]);
    let durations: BTreeMap<Vec<String>, u64> = added(&before[0], &after[0])
        .into_iter()
        .map(|(k, (count, _))| (k, count))
        .collect();
    let expected: BTreeMap<Vec<String>, u64> = [
        (incremental.clone(), 2),
        (declined("out_of_order"), 1),
        (declined("depth_cap"), 1),
        (declined("no_fold_state"), 1),
        (declined("files_removed"), 1),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        durations, expected,
        "one sample per derive, keyed by its path"
    );

    let files = added(&before[1], &after[1]);
    let size = added(&before[2], &after[2]);
    let counts = |points: &BTreeMap<Vec<String>, (u64, f64)>| -> BTreeMap<Vec<String>, u64> {
        points
            .iter()
            .map(|(k, &(count, _))| (k.clone(), count))
            .collect()
    };
    assert_eq!(counts(&files), expected, "one files sample per derive");
    assert_eq!(counts(&size), expected, "one size sample per derive");
    let (rejected, full) = (driven.rejected, driven.full_after_rejection);
    assert_eq!(
        (
            files[&declined("out_of_order")],
            size[&declined("out_of_order")]
        ),
        (
            (1, (rejected.0 + full.0) as f64),
            (1, (rejected.1 + full.1) as f64)
        ),
        "the rejected delta read counts beside the full fold",
    );
    let [first, second] = driven.deltas;
    assert_eq!(
        (files[&incremental], size[&incremental]),
        (
            (2, (first.0 + second.0) as f64),
            (2, (first.1 + second.1) as f64)
        ),
        "each incremental derive reads only its delta",
    );
}

/// A store whose miss has published an artifact folded from one audit
/// file at `TS0`; its tenant audit directory.
fn published_store(path: &Path) -> std::path::PathBuf {
    write_audit(path, &[widened(1, 1, TS0)]);
    write_records(path, &[mined(1, TS0 + 2)]);
    assert_eq!(body_query(path).rows, 1, "the miss publishes");
    path.join("audit").join(format!("tenant_id={TENANT}"))
}

/// Write `event` as one audit file and rename it to `name` in its
/// partition directory, so the test fixes the delta's read order.
fn write_audit_named(path: &Path, audit_dir: &Path, event: AuditEvent, name: &str) -> PathBuf {
    let before = walk_files(audit_dir);
    write_audit(path, &[event]);
    let written = walk_files(audit_dir)
        .into_iter()
        .find(|p| !before.contains(p))
        .expect("a new audit file");
    let named = written.with_file_name(name);
    std::fs::rename(&written, &named).expect("rename audit file");
    named
}

fn tear(file: &Path) {
    let bytes = std::fs::read(file).expect("read audit file");
    std::fs::write(file, &bytes[..bytes.len() / 2]).expect("tear audit file");
}

/// Scenario RFC0033.13 — a stale derive that fails after the guard chose
/// its path keeps that path. A `depth_cap` fallback whose full fold hits a
/// torn file, and a delta whose first file holds an out-of-order event and
/// whose second is torn, each record one duration sample with
/// `mode = full`, the fallback reason and `error.type`, and no files or
/// size sample.
fn failure_after_decision_act(
    guard: &ourios_telemetry::TelemetryGuard,
    exporter: &opentelemetry_sdk::metrics::InMemoryMetricExporter,
) {
    type Setup = fn(&Path, &Path);
    let cases: [(&str, Setup); 2] = [
        ("depth_cap", |path, audit_dir| {
            rewrite_artifact(&audit_dir.join(TEMPLATE_MAP_FILENAME), |json| {
                json["fold_state"]["incremental_depth"] = 256.into();
            });
            let file = write_audit_named(path, audit_dir, widened(1, 2, TS0 + 10), "1.parquet");
            tear(&file);
        }),
        ("out_of_order", |path, audit_dir| {
            write_audit_named(path, audit_dir, widened(1, 2, TS0), "0.parquet");
            let file = write_audit_named(path, audit_dir, widened(1, 3, TS0 + 10), "1.parquet");
            tear(&file);
        }),
    ];
    for (fallback_reason, setup) in cases {
        let bucket = tempfile::tempdir().expect("temp dir");
        let audit_dir = published_store(bucket.path());
        setup(bucket.path(), &audit_dir);
        let before = derive_points(guard, exporter);
        try_body_query(bucket.path()).expect_err("the fallback's read fails the query");
        let after = derive_points(guard, exporter);

        let failed = key(&[
            "error.type=storage".to_string(),
            fallback(fallback_reason),
            mode("full"),
            reason("stale"),
        ]);
        let durations: Vec<(Vec<String>, u64)> = added(&before[0], &after[0])
            .into_iter()
            .map(|(k, (count, _))| (k, count))
            .collect();
        assert_eq!(durations, vec![(failed, 1)], "{fallback_reason}");
        assert!(
            added(&before[1], &after[1]).is_empty() && added(&before[2], &after[2]).is_empty(),
            "{fallback_reason}: a failed derive records no files or size",
        );
    }
}
