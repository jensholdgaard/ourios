//! The PR-gated Docker tests (`loki-interop` CI job): RFC0031.1
//! result-set equivalence and the #538 backdated wide-time-range arm.

use crate::*;

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
