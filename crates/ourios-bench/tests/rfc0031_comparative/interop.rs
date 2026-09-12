//! The PR-gated Docker tests (`loki-interop` CI job): RFC0031.1
//! result-set equivalence and the #538 backdated wide-time-range arm.

use crate::*;

/// Scenario RFC0031.10's machine-check: the comparative Loki
/// configuration indexes only the declared low-cardinality label set.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// The criterion asks for "a test [that] asserts the label set is drawn
/// from a declared low-cardinality allowlist and that `trace_id`,
/// `span_id`, and any per-template id are **absent**". Issue #792: the
/// RFC reached `accepted` with that test an ignored `todo!()`, so the
/// property held only by accident of the stock config — nothing failed if
/// a later edit added a label.
///
/// It matters more than its size suggests. The published L-gate ratios are
/// only meaningful if Loki's side was configured fairly, and the two ways
/// to rig it are both label-shaped: promote a high-cardinality key and
/// Loki's index starts doing Ourios's pruning for it (an L3 trace win over
/// a Loki that indexes `trace_id` measures nothing), or leave only a
/// catch-all and every query degrades to a full scan.
///
/// Asserted against a **running container** on the exact dispatch config,
/// not against the config text: what matters is the label set Loki
/// actually ends up with after an OTLP push, including anything the image
/// promotes on its own.
#[test]
#[ignore = "RFC0031.10 — needs Docker (real Loki container); run by the loki-interop CI job via --ignored"]
fn rfc0031_10_loki_label_allowlist() {
    let records = two_service_fixture();
    let mut logs = fixture_logs_data(&records);
    inject_probe_attributes(&mut logs);
    assert_denylisted_keys_are_on_the_wire(&logs);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        use prost::Message as _;

        let (_container, base, http) = start_loki(LOKI_DISPATCH_FLAGS).await;
        let payload = opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
            resource_logs: logs.resource_logs,
        }
        .encode_to_vec();
        push_otlp(&http, &base, payload).await;

        let (observed, services) = poll_until_both_services_indexed(&http, &base).await;
        assert_within_allowlist(&observed);
        assert_no_denylisted_label(&observed);
        // Not a catch-all: the allowlisted label must actually partition the
        // corpus, else the single label every query selects on discriminates
        // nothing and Loki is forced into a full scan.
        assert_eq!(
            services.len(),
            2,
            "`service_name` must discriminate between the fixture's two \
             services: {services:?}",
        );
    });
}

/// Two services, so `service_name` is provably a discriminator rather than a
/// constant. The other fixtures here carry one service, and a single-valued
/// label is exactly the catch-all case RFC0031.10 rules out.
fn two_service_fixture() -> Vec<FixtureRecord> {
    let base_ns = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos(),
    )
    .expect("nanos fit u64")
    .saturating_sub(30_000_000_000);
    ["checkout", "payment"]
        .into_iter()
        .enumerate()
        .map(|(i, service)| FixtureRecord {
            time_unix_nano: base_ns + u64::try_from(i).expect("tiny index") * 1_000_000_000,
            severity_number: 9,
            severity_text: "INFO",
            body: "connection established to peer 10",
            trace_id: Some(FIXTURE_TRACE),
            service,
        })
        .collect()
}

/// Resource attributes the real dispatch corpus carries which are **not** on
/// the denylist, so a future config or image that promoted one of them would
/// be *observed* by the allowlist assertion rather than silently missed.
///
/// A runtime check can only see labels its payload can produce. Without these
/// the test guards promotions of the four denied keys and nothing else —
/// `host.name` becoming a stream label would have passed, and it is exactly
/// the shape that quietly multiplies Loki's index.
const PROBE_RESOURCE_ATTRIBUTES: [(&str, &str); 13] = [
    // A representative slice of Loki's stock resource-attributes-as-index-labels
    // set. Not the whole set: the stock config also caps a stream at 15 label
    // names, and sending every promoted key at once is rejected with
    // `has 16 label names; limit 15` — which is itself how that set was
    // established here rather than taken from documentation.
    ("service.namespace", "shop"),
    ("service.instance.id", "inst-1"),
    ("deployment.environment", "prod"),
    ("cloud.region", "eu-central-1"),
    ("k8s.cluster.name", "cluster-a"),
    ("k8s.namespace.name", "shop"),
    ("k8s.pod.name", "checkout-0"),
    ("k8s.container.name", "checkout"),
    ("container.name", "checkout"),
    ("k8s.job.name", "checkout-job"),
    // Negative controls: these are NOT promoted today, so a future config or
    // image that started promoting one of them fails the allowlist assertion.
    // They are the reason this list exists at all.
    ("host.name", "node-a"),
    ("service.version", "1.4.2"),
    ("telemetry.sdk.name", "opentelemetry"),
];

/// The same idea for record-level attributes, including a deliberately
/// high-cardinality one: promoting `user.id` would be the most damaging
/// possible change to this config, so the probe has to carry it.
const PROBE_RECORD_ATTRIBUTES: [(&str, &str); 3] = [
    ("http.request.method", "GET"),
    ("user.id", "u-90210"),
    ("thread.name", "worker-3"),
];

/// Put every denylisted key on the wire — in the field or attribute slot a
/// real OTLP push would carry it in — alongside a representative set of
/// non-denylisted attributes.
///
/// The denylisted half matters because without it the denylist loop passes
/// for three of its four names no matter what the config does: those keys
/// were never sent, so they could not have become labels. A safeguard that
/// cannot fail is not one.
///
/// The non-denylisted half matters for the same reason one level up: the
/// allowlist assertion can only reject labels the payload could have
/// produced, so the probe carries resource, record and scope attributes from
/// the shapes the dispatch corpus actually contains.
fn inject_probe_attributes(logs: &mut opentelemetry_proto::tonic::logs::v1::LogsData) {
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;

    const TEMPLATE_ID_KEYS: [&str; 2] = ["template_id", "ourios_template_id"];
    const SPAN_ID: [u8; 8] = [0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11];

    for resource_logs in &mut logs.resource_logs {
        if let Some(resource) = resource_logs.resource.as_mut() {
            resource
                .attributes
                .extend(TEMPLATE_ID_KEYS.map(|k| string_attribute(k, "4711")));
            resource
                .attributes
                .extend(PROBE_RESOURCE_ATTRIBUTES.map(|(key, value)| string_attribute(key, value)));
        }
        for scope in &mut resource_logs.scope_logs {
            // Scope attributes are a third promotion surface, and the shared
            // fixture leaves the scope unset.
            scope.scope = Some(InstrumentationScope {
                name: "checkout.handler".to_string(),
                version: "1.0.0".to_string(),
                attributes: vec![string_attribute("code.namespace", "checkout")],
                ..InstrumentationScope::default()
            });
            for record in &mut scope.log_records {
                record.span_id = SPAN_ID.to_vec();
                record
                    .attributes
                    .extend(TEMPLATE_ID_KEYS.map(|k| string_attribute(k, "4711")));
                record.attributes.extend(
                    PROBE_RECORD_ATTRIBUTES.map(|(key, value)| string_attribute(key, value)),
                );
            }
        }
    }
}

/// Poll until Loki has indexed **both** services, returning the label names
/// and `service_name`'s values.
///
/// Waiting for both is what makes the assertions sound, not merely
/// non-flaky: the push carries two `ResourceLogs` streams and the second can
/// land later, so stopping at the first non-empty answer would check the
/// allowlist against a half-indexed label set.
async fn poll_until_both_services_indexed(
    http: &reqwest::Client,
    base: &str,
) -> (Vec<String>, Vec<String>) {
    let mut observed = Vec::new();
    let mut services = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        // Readiness FIRST, then the label names. Fetching names before
        // establishing readiness loses the race the other way round: if the
        // second stream's index update lands between the two requests, the
        // values answer says "ready" while the names snapshot was taken
        // before it, so the loop would break on a set that is stale or empty
        // — and the allowlist would then be checked against labels the
        // second stream had not yet contributed.
        services = loki_label_values(http, base, "service_name").await;
        if services.len() >= 2 {
            observed = loki_label_names(http, base).await;
            // The names endpoint can itself lag the values endpoint, so only
            // a post-readiness answer that actually carries `service_name`
            // counts as the complete set.
            if observed.iter().any(|name| name == "service_name") {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        !observed.is_empty(),
        "Loki reported no stream labels at all after the push — every \
         assertion over the set would pass vacuously, so this is a failure",
    );
    (observed, services)
}

/// Every indexed label is one RFC0031.10 declared.
fn assert_within_allowlist(observed: &[String]) {
    let unexpected: Vec<&String> = observed
        .iter()
        .filter(|name| !LOKI_LABEL_ALLOWLIST.contains(&name.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "the comparative Loki config indexed labels outside the RFC0031.10 \
         allowlist {LOKI_LABEL_ALLOWLIST:?}: {unexpected:?}. Either the config \
         gained a label promotion (fix the config) or the image now promotes it \
         by default (widen the allowlist in the same commit that re-publishes \
         the affected §9 rows, and say so) — do not widen it silently.",
    );
}

/// None of the high-cardinality keys Ourios prunes on is in Loki's index.
fn assert_no_denylisted_label(observed: &[String]) {
    for forbidden in LOKI_LABEL_DENYLIST {
        assert!(
            !observed.iter().any(|name| name == forbidden),
            "`{forbidden}` is indexed as a Loki stream label; every published \
             L-gate ratio measured against this config is invalid, because \
             Loki's index is doing the pruning the comparison attributes to \
             Ourios",
        );
    }
}

/// An OTLP string attribute.
fn string_attribute(key: &str, value: &str) -> opentelemetry_proto::tonic::common::v1::KeyValue {
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..KeyValue::default()
    }
}

/// Assert the payload really carries every `LOKI_LABEL_DENYLIST` name, so a
/// future edit that drops one of the injections turns the corresponding
/// denylist assertion back into a vacuous pass *loudly* rather than silently.
///
/// This is the limit of what is checkable locally: that the keys went out.
/// Whether Loki then indexed them is exactly what the denylist loop against
/// the live `/labels` answer decides.
fn assert_denylisted_keys_are_on_the_wire(logs: &opentelemetry_proto::tonic::logs::v1::LogsData) {
    let records = || {
        logs.resource_logs
            .iter()
            .flat_map(|rl| rl.scope_logs.iter())
            .flat_map(|sl| sl.log_records.iter())
    };
    let resource_keys: Vec<&str> = logs
        .resource_logs
        .iter()
        .filter_map(|rl| rl.resource.as_ref())
        .flat_map(|r| r.attributes.iter())
        .map(|kv| kv.key.as_str())
        .collect();
    let record_keys: Vec<&str> = records()
        .flat_map(|r| r.attributes.iter())
        .map(|kv| kv.key.as_str())
        .collect();
    for forbidden in LOKI_LABEL_DENYLIST {
        let on_the_wire = match *forbidden {
            // Carried in dedicated protobuf fields, not as attributes.
            "trace_id" => records().all(|r| !r.trace_id.is_empty()),
            "span_id" => records().all(|r| !r.span_id.is_empty()),
            key => resource_keys.contains(&key) && record_keys.contains(&key),
        };
        assert!(
            on_the_wire,
            "the fixture must SEND `{forbidden}` for the denylist assertion on \
             it to mean anything; it is absent from the payload",
        );
    }
}

/// Scenario RFC0031.1 — result-set equivalence gates every comparison.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// The full equivalence harness, end to end: the shared OTLP fixture is
/// ingested by **both** systems — Ourios via the registry-bearing
/// comparative store (in-process querier per RFC 0031 §7), Loki via its
/// native OTLP endpoint on a real container — queried equivalently
/// (logs DSL ↔ `LogQL`), and the two `LineKey` multisets must be
/// identical. A deliberately narrower `LogQL` then asserts the
/// mismatch arm reports `Mismatch` rather than silently passing.
///
/// Plain `#[test]` by design: `ourios_query_lines` owns its own tokio
/// runtime, so the Ourios half runs sync and only the container half
/// runs inside `block_on` (nesting the two would panic).
#[test]
#[ignore = "RFC0031.1 — needs Docker (real Loki container); run by the loki-interop CI job via --ignored"]
fn rfc0031_1_result_set_equivalence() {
    // ------------------------------------------------------------------
    // Shared fixture, stamped near now: Loki's default reject_old_samples
    // refuses lines older than its window, so the base must be recent.
    // ------------------------------------------------------------------
    let base_ns = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos(),
    )
    .expect("nanos fit u64")
    .saturating_sub(30_000_000_000); // 30 s ago (total even on an absurd clock)
    let records = comparative_fixture(base_ns);

    // ------------------------------------------------------------------
    // Ourios half (sync, locally-proven path): fixture → JSONL corpus →
    // registry-bearing store → in-process query → LineKeys.
    // ------------------------------------------------------------------
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
    // The DSL mirrors the LogQL stream selector ({service_name=FIXTURE_
    // SERVICE}) explicitly now that the fixture spans two services — the
    // pair stays equivalent by construction rather than by the corpus
    // happening to be single-service.
    let ourios_lines = ourios_query_lines(
        bucket.path(),
        &tenant,
        &format!("service == \"{FIXTURE_SERVICE}\" and severity >= 0 | limit 1000"),
        now,
        window,
    )
    .expect("ourios extraction");
    assert_eq!(
        ourios_lines.len(),
        3,
        "Ourios returns every FIXTURE_SERVICE line"
    );

    // ------------------------------------------------------------------
    // Loki half (async): container → OTLP push → LogQL → LineKeys.
    // ------------------------------------------------------------------
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (loki_all, loki_narrow, loki_trace) = runtime.block_on(loki_round_trip(&records, base_ns));

    // ------------------------------------------------------------------
    // The equivalence check itself (RFC0031.1): identical multisets for
    // the equivalent query pair; Mismatch for the narrower one.
    // ------------------------------------------------------------------
    let outcome = compare_lines(&ourios_lines, &loki_all, 8);
    assert!(
        outcome.is_equal(),
        "RFC0031.1 — the two systems' answers must be multiset-identical: {outcome:?}",
    );
    assert!(
        !compare_lines(&ourios_lines, &loki_narrow, 8).is_equal(),
        "the deliberately-narrower LogQL must report Mismatch, not silently pass",
    );

    // L3 (trace-correlation) equivalence on the fixture: DSL
    // `trace_id == …` and the LogQL structured-metadata filter must
    // return the same three lines — the cheap cross-system validation of
    // the RFC 0031 L3 pair's query shapes.
    let ourios_trace = ourios_query_lines(
        bucket.path(),
        &tenant,
        &format!("trace_id == \"{FIXTURE_TRACE}\" | limit 1000"),
        now,
        window,
    )
    .expect("ourios trace extraction");
    assert_eq!(ourios_trace.len(), 3, "all FIXTURE_TRACE lines match");
    let trace_outcome = compare_lines(&ourios_trace, &loki_trace, 8);
    assert!(
        trace_outcome.is_equal(),
        "L3 arm — the two systems' trace answers must be multiset-identical: {trace_outcome:?}",
    );
}

/// The Loki half of RFC0031.1: start a real Loki container, push the SAME
/// `LogsData` value the Ourios corpus was rendered from over the native
/// OTLP endpoint, then answer three `LogQL` queries — the
/// fixture-equivalent one (all `FIXTURE_SERVICE` lines), a deliberately
/// narrower one (the mismatch arm), and the cross-stream trace filter
/// (the L3 arm).
pub(crate) async fn loki_round_trip(
    records: &[FixtureRecord],
    base_ns: u64,
) -> (Vec<LineKey>, Vec<LineKey>, Vec<LineKey>) {
    use prost::Message as _;

    let (_container, base, http) = start_loki(&[]).await;

    // Push the SAME LogsData value the Ourios corpus was rendered from,
    // as the OTLP/HTTP protobuf body Loki's endpoint takes.
    let payload = opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
        resource_logs: fixture_logs_data(records).resource_logs,
    }
    .encode_to_vec();
    push_otlp(&http, &base, payload).await;

    // Query until every line is visible (ingest is async); then run the
    // deliberately-narrower query for the mismatch arm.
    let (start, end) = (base_ns, base_ns + 10_000);
    let all_logql = format!("{{service_name=\"{FIXTURE_SERVICE}\"}}");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let loki_all = loop {
        let lines = loki_query_range(&http, &base, &all_logql, start, end).await;
        if lines.len() >= 3 {
            break lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "loki returned {} of 3 fixture lines before timeout",
            lines.len(),
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    let narrow_logql = format!("{{service_name=\"{FIXTURE_SERVICE}\"}} |= \"logged in\"");
    let loki_narrow = loki_query_range(&http, &base, &narrow_logql, start, end).await;
    // Pin the narrow result to exactly the 2 "logged in" lines: the
    // mismatch arm asserts only inequality, so a silently-broken filter
    // returning 0 lines would otherwise still "pass" it.
    assert_eq!(
        loki_narrow.len(),
        2,
        "the narrower filter must match exactly the two 'logged in' lines",
    );

    // L3 arm: Loki's OTLP ingest lands `trace_id` in structured metadata
    // as lowercase hex; this filter is the LogQL half of the RFC 0031 L3
    // (trace-correlation) pair, validated here on the fixture so a wrong
    // metadata key name fails this PR-gated job, not a 40-minute
    // dispatch run. The `.+` selector is deliberate — a trace spans
    // services, so the honest Loki query cannot pre-narrow to one stream.
    let trace_logql = format!("{{service_name=~\".+\"}} | trace_id=\"{FIXTURE_TRACE}\"");
    // The readiness loop above only proves the FIXTURE_SERVICE stream is
    // visible; the service-B stream can land later, so the L3 arm polls
    // to its own deadline before asserting.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let loki_trace = loop {
        let lines = loki_query_range(&http, &base, &trace_logql, start, end).await;
        if lines.len() >= 3 {
            break lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the trace filter returned {} of 3 FIXTURE_TRACE lines before \
             timeout (a wrong structured-metadata key returns 0; an \
             accidentally-narrowed selector returns 2)",
            lines.len(),
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(
        loki_trace.len(),
        3,
        "the trace filter must match all three FIXTURE_TRACE lines ACROSS \
         both service streams, never more",
    );
    (loki_all, loki_narrow, loki_trace)
}

/// The backdated wide-time-range fixture (issue #538 item 2): nine
/// records, 12 h apart, spanning ~4 days — every timestamp far beyond
/// Loki's default 3 h `query_ingesters_within` cutoff and its default
/// `reject_old_samples` window, i.e. the exact ingester-vs-store query
/// routing regime the frozen-corpus dispatch lives in (and where the
/// L3 0-of-N flicker and the L4 completeness loss were found). One
/// service; all records share [`FIXTURE_TRACE`] (the L3-shaped arm);
/// bodies alternate two `peer` values of one template (the L4-shaped
/// arm: cardinality 2, ≥ 4 rows, multiple 12 h buckets, and a
/// per-bucket cadence far above `L4_MIN_AVG_INTERVAL_SECONDS`).
pub(crate) fn backdated_wide_range_fixture(base_ns: u64) -> Vec<FixtureRecord> {
    const TWELVE_HOURS_NS: u64 = 12 * 3600 * 1_000_000_000;
    (0..9u64)
        .map(|i| FixtureRecord {
            time_unix_nano: base_ns + i * TWELVE_HOURS_NS,
            severity_number: 9,
            severity_text: "INFO",
            body: if i % 2 == 0 {
                "connection established to peer 10"
            } else {
                "connection established to peer 11"
            },
            trace_id: Some(FIXTURE_TRACE),
            service: FIXTURE_SERVICE,
        })
        .collect()
}

/// Issue #538 item 2 — the backdated wide-time-range arm of the Loki
/// interop job. The plain RFC0031.1 test stamps its fixture ~30 s ago,
/// so it never exercises the query-routing regime the real dispatch
/// runs in: a frozen corpus whose entire time range is days old, where
/// Loki's ingester-vs-store routing decides whether unflushed rows are
/// visible at all. That regime is where both characterized dispatch
/// failure modes live — the L3 trace pair's 0-of-N flicker (runs
/// #20/#22: `query_ingesters_within` routing) and the L4 wide-range
/// completeness loss. This test pins, per-PR and in ~1 minute, that
/// the EXACT dispatch Loki config ([`LOKI_DISPATCH_FLAGS`], shared by
/// construction) returns complete, equivalent answers for both shapes
/// over a backdated multi-day range:
///
/// - an L3-shaped trace-correlation query (9 rows, one trace, ~4-day
///   window) polled to completeness — a plateau below 9 is the routing
///   flake reproduced at fixture scale;
/// - an L4-shaped `count_over_time` matrix query, exact-equivalent to
///   Ourios's grouped counts (`compare_aggregations`, no margin — at
///   fixture scale completeness has never been observed to fall short,
///   so exact is the honest assertion; if the corpus-scale loss ever
///   reproduces down here, this failing IS the discovery).
#[test]
#[ignore = "RFC 0031 / #538 item 2 — needs Docker (real Loki container); run by the loki-interop CI job via --ignored"]
fn rfc0031_backdated_wide_range_interop() {
    let now_ns = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos(),
    )
    .expect("nanos fit u64");
    // End the span ~1 h ago so every record is stale relative to `now`,
    // start it ~4 days + 1 h ago.
    let base_ns = now_ns.saturating_sub(4 * 24 * 3600 * 1_000_000_000 + 3600 * 1_000_000_000);
    let records = backdated_wide_range_fixture(base_ns);

    // Ourios half: fixture → store → the two query shapes.
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
    let ourios_trace = ourios_query_lines(
        bucket.path(),
        &tenant,
        &format!("trace_id == \"{FIXTURE_TRACE}\" | limit 1000"),
        now,
        window,
    )
    .expect("ourios trace extraction");
    assert_eq!(ourios_trace.len(), 9, "Ourios returns every fixture line");
    let frequency = pick_frequency_pair(bucket.path(), &tenant, now, window)
        .expect("the peer template must yield an L4 candidate on this fixture");
    let margins = ourios_bench::ComparativeMargins::default();
    let l4_spec = l4_pair_spec(
        &frequency,
        built.min_effective_time_unix_nano,
        built.max_effective_time_unix_nano,
        now,
        window,
        &margins,
    )
    .expect("the L4 pair spec must build (no backtick in the capture regex)");
    let bucket_width_ns = bucket_width_seconds(&frequency.bucket_width)
        .checked_mul(1_000_000_000)
        .expect("bucket width fits u64 nanoseconds");

    // Loki half: the DISPATCH config, a backdated push, both shapes
    // polled to completeness.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (loki_trace, loki_groups) = runtime.block_on(backdated_loki_answers(
        &records,
        base_ns,
        now_ns,
        &l4_spec,
        bucket_width_ns,
        frequency.groups.values().sum(),
    ));

    let trace_outcome = compare_lines(&ourios_trace, &loki_trace, 8);
    assert!(
        trace_outcome.is_equal(),
        "backdated L3 arm — the two systems' trace answers must be \
         multiset-identical: {trace_outcome:?}",
    );
    let agg_outcome = ourios_bench::compare_aggregations(&frequency.groups, &loki_groups, 8);
    assert!(
        agg_outcome.is_equal(),
        "backdated L4 arm — the grouped counts must be EXACTLY equal at \
         fixture scale (no completeness margin down here): {agg_outcome:?}",
    );
}

/// The Loki half of [`rfc0031_backdated_wide_range_interop`]: the
/// DISPATCH config ([`LOKI_DISPATCH_FLAGS`]), one backdated OTLP push,
/// then both query shapes polled to completeness — the trace filter to
/// all 9 rows, the L4 matrix to `expected` rows. A plateau below either
/// target is the corresponding dispatch failure mode reproduced at
/// fixture scale.
pub(crate) async fn backdated_loki_answers(
    records: &[FixtureRecord],
    base_ns: u64,
    now_ns: u64,
    l4_spec: &PairSpec,
    bucket_width_ns: u64,
    expected: u64,
) -> (Vec<LineKey>, HashMap<AggKey, u64>) {
    use prost::Message as _;
    let (_container, base, http) = start_loki(LOKI_DISPATCH_FLAGS).await;
    let payload = opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
        resource_logs: fixture_logs_data(records).resource_logs,
    }
    .encode_to_vec();
    push_otlp(&http, &base, payload).await;

    let trace_logql = format!("{{service_name=~\".+\"}} | trace_id=\"{FIXTURE_TRACE}\"");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let loki_trace = loop {
        let lines = loki_query_range(&http, &base, &trace_logql, base_ns, now_ns).await;
        if lines.len() >= 9 {
            break lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "backdated trace query plateaued at {} of 9 rows — the \
             ingester-vs-store routing flake reproduced at fixture scale \
             (or LOKI_DISPATCH_FLAGS' routing config regressed)",
            lines.len(),
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let loki_groups = loop {
        match loki_query_matrix(
            &http,
            &base,
            &l4_spec.logql,
            l4_spec.start,
            l4_spec.end,
            bucket_width_ns,
            "value",
        )
        .await
        {
            Ok((groups, _, _)) if groups.values().sum::<u64>() >= expected => break groups,
            Ok((groups, _, _)) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "backdated matrix query plateaued at {} of {expected} rows — \
                     the wide-range completeness loss reproduced at fixture scale",
                    groups.values().sum::<u64>(),
                );
            }
            Err(detail) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "backdated matrix query kept failing: {detail}",
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    (loki_trace, loki_groups)
}
