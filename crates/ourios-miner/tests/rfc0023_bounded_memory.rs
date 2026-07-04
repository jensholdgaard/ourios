//! RFC 0023 §5 — bounded template memory (RFC 0001 amendment).
//!
//! Scenarios RFC0023.1/.3/.4/.5 live here (miner-level bounds +
//! corpus invariance); RFC0023.6 (telemetry) stays stubbed for the
//! telemetry green slice. RFC0023.2 (overflow bodies round-trip
//! through the Parquet body column) is an ingest-path integration
//! and lives in
//! `crates/ourios-ingester/tests/rfc0023_overflow_roundtrip.rs`.
//! RFC0023.7 (the 16 GiB `HDFS_v2` scale rerun under 8 GiB peak RSS)
//! is a bench-hardware criterion discharged by the
//! `docs/benchmarks.md` §9 record, not a `cargo test` — the runner
//! lives in the maintainer's `scratch/baseline/` tooling.

use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};

fn record(tenant: &TenantId, text: &str) -> OtlpLogRecord {
    OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    }
}

/// The tenant's template set as canonical `(rendered_template, id)`
/// pairs, sorted — the identity oracle RFC0023.1/.5 compare on.
/// Rendering goes through the miner's own [`format_template`]
/// (`ourios_miner::tree`), the stable canonical form, not `Debug`.
fn template_set(cluster: &MinerCluster, tenant: &TenantId) -> Vec<(String, u64)> {
    let mut set: Vec<(String, u64)> = cluster
        .templates_for(tenant)
        .into_iter()
        .map(|leaf| {
            (
                ourios_miner::tree::format_template(&leaf.template),
                leaf.template_id,
            )
        })
        .collect();
    set.sort();
    set
}

/// RFC 0023 §3.1 — a zero bound is a startup configuration error
/// (`BoundZero` names the field), never a runtime divert-everything
/// state. Companion to the RFC 0004 validation suite in
/// `tests/invariants.rs`.
#[test]
fn zero_bounds_are_rejected_at_startup() {
    use ourios_core::config::MinerConfigError;
    assert_eq!(
        MinerConfig::default().with_max_node_children(0),
        Err(MinerConfigError::BoundZero("max_node_children")),
    );
    assert_eq!(
        MinerConfig::default().with_max_templates(0),
        Err(MinerConfigError::BoundZero("max_templates")),
    );
    assert_eq!(
        MinerConfig::default().with_max_line_tokens(0),
        Err(MinerConfigError::BoundZero("max_line_tokens")),
    );
}

/// Scenario RFC0023.1 — the ceiling holds and never merges.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
fn rfc0023_1_template_ceiling_holds_and_never_merges() {
    // Six structurally distinct lines (distinct token counts ⇒
    // distinct length buckets ⇒ every one would mint uncapped).
    let lines = [
        "alpha started",
        "beta accepted request now",
        "gamma rejected request from peer five",
        "delta flushed segment to disk at offset nine",
        "epsilon compacted partition into one file at generation two",
        "zeta rotated wal segment after threshold breach on node three today",
    ];
    let tenant = TenantId::new("capped");

    let ceiling = 3u32;
    let sink = SharedRecordSink::new();
    let config = MinerConfig::default()
        .with_max_templates(ceiling)
        .expect("non-zero ceiling");
    let mut capped = MinerCluster::new(config).with_record_sink(Box::new(sink.clone()));
    let mut uncapped = MinerCluster::new(MinerConfig::default());

    let mut capped_ids = Vec::new();
    for line in lines {
        capped_ids.push(capped.ingest(&record(&tenant, line)));
        uncapped.ingest(&record(&tenant, line));
    }

    // The count plateaus at the ceiling…
    assert_eq!(capped.templates_for(&tenant).len(), ceiling as usize);
    // …every would-mint line diverted to parse-failure (no template,
    // body retained, lossy-flagged), never attached to an existing
    // template.
    let emitted = sink.drain();
    assert_eq!(emitted.len(), lines.len());
    for (i, (line, rec)) in lines.iter().zip(&emitted).enumerate() {
        if i < ceiling as usize {
            assert_ne!(rec.template_id, NO_TEMPLATE, "line {i} minted");
        } else {
            assert_eq!(capped_ids[i], NO_TEMPLATE, "line {i} diverted");
            assert_eq!(rec.template_id, NO_TEMPLATE, "line {i} carries no template");
            assert!(rec.lossy_flag, "line {i} is lossy-flagged");
            assert_eq!(rec.body.as_deref(), Some(*line), "line {i} body retained");
        }
    }
    // The capped set is exactly the uncapped run truncated at the
    // ceiling — the ceiling stopped growth without disturbing what
    // was already mined.
    let uncapped_first: Vec<_> = template_set(&uncapped, &tenant)
        .into_iter()
        .filter(|(_, id)| capped_ids.contains(id))
        .collect();
    assert_eq!(template_set(&capped, &tenant), uncapped_first);
}

/// Scenario RFC0023.3 — node fan-out caps via wildcard routing.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
fn rfc0023_3_node_fanout_caps_via_wildcard_routing() {
    let tenant = TenantId::new("fanout");
    let config = MinerConfig::default()
        .with_max_node_children(2)
        .expect("non-zero cap");
    let mut cluster = MinerCluster::new(config);

    // Four distinct first tokens at one prefix level (same token
    // count ⇒ one length bucket). The first two branch keyed; the
    // rest route through the wildcard child — and still mint their
    // own leaves (routing is not merging).
    let first = cluster.ingest(&record(&tenant, "alpha handler finished cleanly"));
    let second = cluster.ingest(&record(&tenant, "beta handler finished cleanly"));
    let third = cluster.ingest(&record(&tenant, "gamma worker crashed hard"));
    let fourth = cluster.ingest(&record(&tenant, "delta poller idled quietly"));
    let minted: std::collections::HashSet<u64> = [first, second, third, fourth].into();
    assert_eq!(minted.len(), 4, "all four lines mint distinct templates");
    assert!(!minted.contains(&NO_TEMPLATE));

    // Attach through the wildcard route: an exact repeat of a
    // wildcard-routed line reuses its leaf (read-side routing finds
    // the same bucket the write side used).
    let repeat = cluster.ingest(&record(&tenant, "gamma worker crashed hard"));
    assert_eq!(repeat, third, "wildcard-routed repeat attaches cleanly");

    // Attach below the wildcard child stays threshold-gated: a
    // below-floor line landing in gamma's exact bucket (unseen first
    // token ⇒ wildcard route; second token "worker" ⇒ gamma's
    // prefix path; same length) matches gamma's leaf at 1/4 < the
    // 0.4 floor and must fail parse — not merge, not mint.
    let templates_before = cluster.templates_for(&tenant).len();
    let unrelated = cluster.ingest(&record(&tenant, "omega worker jumped nowhere"));
    assert_eq!(
        unrelated, NO_TEMPLATE,
        "below-floor under the wildcard child fails parse rather than merging",
    );
    assert_eq!(
        cluster.templates_for(&tenant).len(),
        templates_before,
        "the parse failure minted nothing",
    );
}

/// Scenario RFC0023.4 — the long-line guard.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
fn rfc0023_4_long_lines_fail_parse_with_body_retained() {
    let tenant = TenantId::new("longline");
    let sink = SharedRecordSink::new();
    let config = MinerConfig::default()
        .with_max_line_tokens(4)
        .expect("non-zero cap");
    let mut cluster = MinerCluster::new(config).with_record_sink(Box::new(sink.clone()));

    let long = "one two three four five";
    let id = cluster.ingest(&record(&tenant, long));
    assert_eq!(
        id, NO_TEMPLATE,
        "over-cap line takes the parse-failure path"
    );
    let mut emitted = sink.drain();
    assert_eq!(emitted.len(), 1, "exactly one record per ingested line");
    let rec = emitted.pop().expect("asserted non-empty above");
    assert!(rec.lossy_flag);
    assert_eq!(
        rec.body.as_deref(),
        Some(long),
        "body retained bit-identically"
    );
    assert!(
        cluster.templates_for(&tenant).is_empty(),
        "no template of that width exists in the tree",
    );

    // At the cap exactly, the line mines normally.
    let ok = cluster.ingest(&record(&tenant, "one two three four"));
    assert_ne!(ok, NO_TEMPLATE);
}

/// Scenario RFC0023.5 — defaults are invisible on healthy corpora.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
///
/// The seed corpus (`testdata/corpus/`) mines to an identical
/// template set under the default bounds and under bounds set to
/// their type maxima (the closest expressible "unbounded" run).
/// The full-strength oracle stays the corpus/C1/C2 suites in CI,
/// which now run under the defaults.
#[test]
fn rfc0023_5_default_bounds_are_invisible_on_healthy_corpora() {
    use std::io::BufRead;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root is two levels above CARGO_MANIFEST_DIR")
        .join("testdata/corpus");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("seed corpus dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "seed corpus has *.txt files");

    let tenant = TenantId::new("seed");
    let mut defaults = MinerCluster::new(MinerConfig::default());
    let unbounded_config = MinerConfig::default()
        .with_max_node_children(u16::MAX)
        .and_then(|c| c.with_max_templates(u32::MAX))
        .and_then(|c| c.with_max_line_tokens(u16::MAX))
        .expect("maxima are non-zero");
    let mut unbounded = MinerCluster::new(unbounded_config);

    for path in &paths {
        let file = std::fs::File::open(path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read line");
            if line.is_empty() {
                continue;
            }
            defaults.ingest(&record(&tenant, &line));
            unbounded.ingest(&record(&tenant, &line));
        }
    }

    assert_eq!(
        template_set(&defaults, &tenant),
        template_set(&unbounded, &tenant),
        "default bounds must not change what a healthy corpus mines to",
    );
}

/// Scenario RFC0023.6 — saturation is observable.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
///
/// A ceiling-saturated tenant is diagnosable from telemetry alone:
/// `ourios.miner.parse_failures` carries
/// `ourios.miner.parse_failure.reason = template_ceiling` increments
/// and `ourios.miner.template.count` reads the ceiling value.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0023_6_ceiling_saturation_is_observable() {
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };

    fn has_attr<'a>(
        mut attrs: impl Iterator<Item = &'a opentelemetry::KeyValue>,
        key: &str,
        value: &str,
    ) -> bool {
        attrs.any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
    }

    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let ceiling = 1u32;
    let config = MinerConfig::default()
        .with_max_templates(ceiling)
        .expect("non-zero ceiling");
    let mut cluster = MinerCluster::new(config);
    let tenant = TenantId::new("saturated");

    // One mint fills the ceiling; two structurally distinct lines
    // divert with reason = template_ceiling.
    cluster.ingest(&record(&tenant, "alpha started"));
    cluster.ingest(&record(&tenant, "beta accepted request now"));
    cluster.ingest(&record(&tenant, "gamma rejected request from peer five"));
    guard.force_flush().expect("force_flush succeeds");

    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let metric = |name: &str| {
        rms.iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .find(|m| m.name() == name)
            .unwrap_or_else(|| panic!("{name} missing from exported stream"))
            .data()
    };

    // The reason-attributed failure counter.
    let AggregatedMetrics::U64(MetricData::Sum(sum)) =
        metric(ourios_semconv::OURIOS_MINER_PARSE_FAILURES)
    else {
        panic!("parse_failures should be a u64 sum");
    };
    let ceiling_failures: u64 = sum
        .data_points()
        .filter(|dp| {
            has_attr(
                dp.attributes(),
                ourios_semconv::OURIOS_MINER_PARSE_FAILURE_REASON,
                "template_ceiling",
            ) && has_attr(dp.attributes(), ourios_semconv::OURIOS_TENANT, "saturated")
        })
        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
        .sum();
    assert_eq!(
        ceiling_failures, 2,
        "both diverted lines counted under reason = template_ceiling",
    );

    // The gauge reads the ceiling.
    let AggregatedMetrics::U64(MetricData::Gauge(gauge)) =
        metric(ourios_semconv::OURIOS_MINER_TEMPLATE_COUNT)
    else {
        panic!("template.count should be a u64 gauge");
    };
    let count: u64 = gauge
        .data_points()
        .find(|dp| has_attr(dp.attributes(), ourios_semconv::OURIOS_TENANT, "saturated"))
        .map(opentelemetry_sdk::metrics::data::GaugeDataPoint::value)
        .expect("the saturated tenant's data point");
    assert_eq!(u64::from(ceiling), count, "the gauge reads the ceiling");
}
