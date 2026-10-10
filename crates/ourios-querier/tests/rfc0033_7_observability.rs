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
//! This test installs a process-global in-memory `MeterProvider`, so it
//! lives in its own integration binary (its own process) — the
//! `rfc0016_6_query_metrics` precedent, applied at the querier-library
//! surface where the RFC 0033 instruments live — and is one of this
//! crate's two RFC0028.2 process-isolation exemptions (the other,
//! `rfc0033_bounded_fold.rs`, installs a global allocator).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
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
    let rows = runtime.block_on(async {
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let querier = Arc::clone(&querier);
                tokio::spawn(async move {
                    let query =
                        ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
                    querier
                        .run_query(&query, &TenantId::new(TENANT), NOW, DEFAULT_WINDOW_NS, None)
                        .await
                        .expect("shared acquisition answers both queries")
                        .rows
                })
            })
            .collect();
        let mut rows = Vec::new();
        for task in tasks {
            rows.push(task.await.expect("query task"));
        }
        rows
    });
    querier.set_template_map_acquire_gate(None);
    assert_eq!(rows, vec![1, 1]);
    assert_eq!(querier.template_map_waits(), 1, "exactly one query waited");
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

/// The counter `metric`'s data points as `attr_key value → count` — one
/// entry per exported attribute set, so extras (a stray outcome, an
/// attribute-free point) fail the map equality below.
fn outcome_counts(rms: &[ResourceMetrics], metric: &str, attr_key: &str) -> BTreeMap<String, u64> {
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric_data(rms, metric) else {
        panic!("{metric} should be a u64 sum (counter)");
    };
    let mut counts = BTreeMap::new();
    for dp in sum.data_points() {
        let attrs: Vec<_> = dp.attributes().collect();
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
}

/// Scenario RFC0033.7 — observable outcomes.
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

    // Assert — the exported stream carries the §3.7 instruments, every
    // driven outcome recorded once under its registry attribute value.
    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");

    let lookups = outcome_counts(
        &rms,
        semconv::OURIOS_TEMPLATE_MAP_LOOKUPS,
        semconv::OURIOS_TEMPLATE_MAP_LOOKUP_OUTCOME,
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
        &rms,
        semconv::OURIOS_TEMPLATE_MAP_PUBLISHES,
        semconv::OURIOS_TEMPLATE_MAP_PUBLISH_OUTCOME,
    );
    let expected: BTreeMap<String, u64> = [("published".to_string(), 3)].into_iter().collect();
    assert_eq!(publishes, expected, "three write-through publishes");

    // The publish-size histogram recorded each published artifact's
    // byte size (the number RFC0033.6 gates on).
    let AggregatedMetrics::U64(MetricData::Histogram(hist)) =
        metric_data(&rms, semconv::OURIOS_TEMPLATE_MAP_ARTIFACT_SIZE)
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
        size_after_miss + size_after_stale + size_after_torn,
        "each sample is the published artifact's byte size",
    );
    binding_conflict_act(&guard, &exporter, &rms);
}
