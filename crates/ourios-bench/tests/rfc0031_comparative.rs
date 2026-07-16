//! RFC 0031 — comparative evaluation vs Grafana Loki (§5 scenarios).
//!
//! One scenario per §5 acceptance criterion (RFC0031.1–.11). `.1`
//! (result-set equivalence) is **live**: a real Loki container run,
//! `#[ignore]`d because it needs Docker — the `loki-interop` CI job
//! executes it via `--ignored --exact` (the dex-oidc precedent).
//! `.2`/`.4`/`.7`/`.11` are green under the §7 partial freeze
//! (2026-07-13): locally-provable scenarios over the frozen gate math
//! and the published §9.13 record, with the dispatch run asserting the
//! same frozen gates on live measurements. The remaining six are
//! `#[ignore]`d red stubs, each discharged by its named green slice.
//!
//! Placement note: the comparative harness lives in `ourios-bench`
//! for now (extending the RFC 0006 harness) rather than a new crate,
//! keeping the §7 "new crate vs `bench/` harness" question open — a
//! new crate is a `CLAUDE.md` §7 architectural commitment and is not
//! made here.
//!
//! The primary gate metric throughout is **bytes read from object
//! storage** (RFC 0031 §2.5 / §3.6): the implementation-independent
//! expression of the pruning thesis. Latency is corroborating, not
//! sole-gating. See `docs/rfcs/0031-comparative-evaluation-loki.md`.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ourios_bench::{
    AggKey, FIXTURE_SERVICE, FIXTURE_SERVICE_B, FIXTURE_TRACE, FixtureRecord, LineKey,
    LokiFetchedBytes, comparative_fixture, compare_aggregations, compare_lines, fixture_jsonl,
    fixture_logs_data, ourios_aggregate_answer, ourios_query_lines, parse_loki_bytes_processed,
    parse_loki_fetched_bytes, parse_loki_matrix, parse_loki_streams,
};
use ourios_core::tenant::TenantId;
use ourios_miner::tree::OwnedToken;

/// `grafana/loki`, digest-pinned like the Dex image (the tag names the
/// release a competent operator would run; the digest makes CI
/// reproducible).
const LOKI_IMAGE: &str = "grafana/loki";
const LOKI_TAG: &str =
    "3.5.3@sha256:3165cecce301ce5b9b6e3530284b080934a05cd5cafac3d3d82edcb887b45ecd";

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

/// Start a Loki container on the stock image config plus `extra_args`
/// (explicit, documented CLI-flag deviations), wait for `/ready`, and
/// hand back the container (kept alive by the caller), the base URL,
/// and a timeout-bearing HTTP client.
///
/// The stock image config (schema v13 / TSDB) serves the native OTLP
/// endpoint and maps `service.name` → the `service_name` stream label;
/// auth is disabled. Exactly what a competent single-binary operator
/// gets out of the box.
async fn start_loki(
    extra_args: &[&str],
) -> (
    testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::testcontainers::GenericImage,
    >,
    String,
    reqwest::Client,
) {
    use testcontainers_modules::testcontainers::core::ContainerPort;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;
    use testcontainers_modules::testcontainers::{GenericImage, ImageExt};

    let mut cmd = vec!["-config.file=/etc/loki/local-config.yaml"];
    cmd.extend_from_slice(extra_args);
    let container = GenericImage::new(LOKI_IMAGE, LOKI_TAG)
        .with_exposed_port(ContainerPort::Tcp(3100))
        .with_cmd(cmd)
        .start()
        .await
        .expect("loki container starts");
    let port = container
        .get_host_port_ipv4(3100)
        .await
        .expect("loki host port");
    let base = format!("http://127.0.0.1:{port}");
    // A per-request timeout so a wedged container/network stack fails a
    // request (and the surrounding deadline loop moves on) rather than
    // hanging the CI job to its global timeout.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client");

    // Readiness: /ready flips 200 once the ingester is up. Surface the
    // container's own output on timeout so a config rejection doesn't
    // read as a bare timeout (Loki writes startup errors to both streams).
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(r) = http.get(format!("{base}/ready")).send().await
            && r.status().is_success()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let stdout = container
                .stdout_to_vec()
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let stderr = container
                .stderr_to_vec()
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            panic!(
                "loki /ready never turned 200.\n--- loki stdout ---\n{stdout}\n\
                 --- loki stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    (container, base, http)
}

/// POST one OTLP protobuf payload to Loki, retrying transient rejections
/// (429 rate-limit / 5xx) with a short backoff — a sustained-ingest push
/// must not fail the run on a burst limit.
async fn push_otlp(http: &reqwest::Client, base: &str, payload: Vec<u8>) {
    // Fail FAST on an oversized payload: it would 503 permanently (Loki's
    // stock 4 MiB internal gRPC cap), and burning the retry deadline on it
    // masks the real cause. This converts the batcher's byte ESTIMATE into
    // a checked guarantee at the actual encoded size.
    assert!(
        payload.len() < 4 * 1024 * 1024,
        "OTLP payload is {} bytes — at/over Loki's stock 4 MiB gRPC cap; \
         the byte-capped batcher under-estimated",
        payload.len(),
    );
    // `Bytes` so the per-attempt body handoff is a refcount bump, not a
    // multi-MB copy on every push of a long replay.
    let payload = bytes::Bytes::from(payload);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let sent = http
            .post(format!("{base}/otlp/v1/logs"))
            .header("content-type", "application/x-protobuf")
            .body(payload.clone())
            .send()
            .await;
        // Transport errors (connection reset, timeout) are as transient as
        // a 429 during a sustained replay — retry them within the same
        // deadline rather than aborting the run on a blip.
        let (status, body) = match sent {
            Ok(resp) if resp.status().is_success() => {
                // A 2xx can still carry an OTLP partialSuccess with
                // silently-rejected records — which would unequalize the
                // two corpora and surface later as a baffling equivalence
                // or row-count failure. Fail HERE, loudly.
                let body = resp.bytes().await.expect("otlp push response body");
                if !body.is_empty() {
                    use prost::Message as _;
                    let decoded =
                        opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceResponse::decode(
                            body.as_ref(),
                        )
                        .expect("otlp push response decodes");
                    if let Some(partial) = decoded.partial_success {
                        assert!(
                            partial.rejected_log_records == 0,
                            "loki silently rejected {} records: {}",
                            partial.rejected_log_records,
                            partial.error_message,
                        );
                    }
                }
                return;
            }
            Ok(resp) => {
                let status = resp.status();
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let body = resp.text().await.unwrap_or_default();
                assert!(retryable, "loki otlp push rejected: {status} — {body}");
                (status.to_string(), body)
            }
            Err(e) => ("transport error".to_string(), e.to_string()),
        };
        assert!(
            std::time::Instant::now() < deadline,
            "loki otlp push kept failing past the deadline: {status} — {body}",
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The Loki half of RFC0031.1: start a real Loki container, push the SAME
/// `LogsData` value the Ourios corpus was rendered from over the native
/// OTLP endpoint, then answer three `LogQL` queries — the
/// fixture-equivalent one (all `FIXTURE_SERVICE` lines), a deliberately
/// narrower one (the mismatch arm), and the cross-stream trace filter
/// (the L3 arm).
async fn loki_round_trip(
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

/// One Loki `query_range` call, parsed to [`LineKey`]s.
async fn loki_query_range(
    http: &reqwest::Client,
    base: &str,
    logql: &str,
    start: u64,
    end: u64,
) -> Vec<LineKey> {
    let resp = http
        .get(format!("{base}/loki/api/v1/query_range"))
        .query(&[
            ("query", logql),
            ("start", &start.to_string()),
            ("end", &end.to_string()),
            // Loki's stock max_entries_limit_per_query is 5000; the
            // indicative pair picker caps expected rows at 4000, so a
            // complete result always fits one page (no pagination, and
            // the equivalence check can't silently truncate).
            ("limit", "5000"),
            ("direction", "forward"),
        ])
        .send()
        .await
        .expect("query_range");
    // Check the HTTP status before parsing: a non-2xx body may not be the
    // streams JSON at all, and "parse failed" would mask the real error.
    let status = resp.status();
    let body = resp.text().await.expect("query_range body");
    assert!(
        status.is_success(),
        "loki query_range returned {status}: {body}",
    );
    parse_loki_streams(&body).expect("parse loki streams")
}

/// Scenario RFC0031.2 — L1 selective template lookup wins on bytes read.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green under the §7 partial freeze (2026-07-13): `M_L1 = 10` on the
/// storage-side bytes channel. Locally provable without a container —
/// the frozen gate math decides exactly at the margin boundary in the
/// must-win direction (above the ratio flips `pass = false`, the
/// pillar-level-finding arm), and the §9.13-recorded L1 measurements
/// (documented historical evidence from equivalence-verified runs
/// #15–#17, NOT a live measurement) clear the frozen margin. The live
/// assertion of the same gate is `rfc0031_indicative_comparative_run`;
/// latency stays recorded there as corroborating, non-gating.
#[test]
fn rfc0031_2_l1_template_lookup_bytes() {
    let m_l1 = ourios_bench::ComparativeMargins::default().m_l1;
    assert_eq!(m_l1, 10, "the §7-frozen L1 must-win margin");

    // The must-win rule at the frozen margin: pass at and below
    // ourios × 10 ≤ loki, fail one byte above — and a failure is a
    // Decided { pass: false, .. }, the reportable pillar-level arm.
    assert!(ourios_bench::bytes_must_win(100, 1_000, m_l1).passed());
    let over = ourios_bench::bytes_must_win(101, 1_000, m_l1);
    assert!(!over.passed(), "{over:?}");
    assert!(matches!(
        over,
        ourios_bench::BytesGateOutcome::Decided { pass: false, .. }
    ));

    // §9.13 (runs #15/#16/#17): ourios 1,358,683 B vs Loki storage-side
    // 104.8–105.6 MB — 77.2–77.7×, clearing the frozen margin with
    // headroom in every counted run since the pair landed.
    for loki_storage in [104_825_428, 105_191_956, 105_579_510] {
        let outcome = ourios_bench::bytes_must_win(1_358_683, loki_storage, m_l1);
        assert!(
            outcome.passed(),
            "a recorded §9.13 L1 run must clear the frozen M_L1: {outcome:?}",
        );
    }
}

/// Scenario RFC0031.3 — L2 attribute predicate wins on bytes read.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green under the §7 `M_L2` unfreeze (2026-07-14, the named condition
/// met: RFC 0033's v2 artifact merged and measured warm at 187,904 B in
/// run #21, §9.15): `M_L2 = 10` on the processed channel as primary,
/// plus the 1.1× storage-side floor (`m_l2_storage_floor_tenths = 11`).
/// Locally provable without a container — both frozen gates decide
/// exactly at their boundaries in the must-win direction, and the
/// recorded measurements (documented historical evidence from
/// equivalence-verified runs, NOT a live measurement) decide correctly
/// on both sides: the post-artifact honest total (§9.13 run #10's
/// components with §9.15's warm registry: 0 + 2,035,267 + 187,904 =
/// 2,223,171 B) clears both channels against every §9.13 reproduction
/// row, while the pre-RFC-0033 total (2,549,129 B) correctly FAILS the
/// storage floor on the weakest recorded Loki row — the floor is not
/// vacuous. The live assertion of the same gates is
/// `rfc0031_indicative_comparative_run`.
#[test]
fn rfc0031_3_l2_attribute_predicate_bytes() {
    let margins = ourios_bench::ComparativeMargins::default();
    assert_eq!(
        margins.m_l2, 10,
        "the §7-frozen L2 processed-channel margin"
    );
    assert_eq!(
        margins.m_l2_storage_floor_tenths, 11,
        "the §7-frozen L2 storage-side floor (1.1×, in tenths)"
    );

    // The processed must-win rule at the frozen margin: pass at and
    // below ourios × 10 ≤ loki, fail one byte above — a Decided fail,
    // the reportable pillar-level arm.
    assert!(ourios_bench::bytes_must_win(100, 1_000, margins.m_l2).passed());
    let over = ourios_bench::bytes_must_win(101, 1_000, margins.m_l2);
    assert!(!over.passed(), "{over:?}");
    assert!(matches!(
        over,
        ourios_bench::BytesGateOutcome::Decided { pass: false, .. }
    ));
    // The storage floor in tenths: pass at ourios × 11 == loki × 10,
    // fail one byte above.
    let tenths = margins.m_l2_storage_floor_tenths;
    assert!(ourios_bench::bytes_must_win_tenths(1_000, 1_100, tenths).passed());
    let over = ourios_bench::bytes_must_win_tenths(1_001, 1_100, tenths);
    assert!(!over.passed(), "{over:?}");
    assert!(matches!(
        over,
        ourios_bench::BytesGateOutcome::Decided { pass: false, .. }
    ));

    // §9.13 (runs #10–#17 reproductions) with §9.15's warm registry
    // applied: the post-artifact total clears the processed margin
    // (37.3–45.1×) and the storage floor (1.20–1.51×) on every
    // recorded (storage, processed) row.
    let post_artifact = 2_223_171;
    for (loki_storage, loki_processed) in [
        (2_751_834, 85_261_718),
        (2_779_800, 86_255_901),
        (3_349_897, 98_253_343),
        (3_224_893, 100_044_070),
        (2_688_942, 83_216_895),
        (2_673_545, 82_919_233),
        (3_224_528, 100_198_466),
    ] {
        let processed = ourios_bench::bytes_must_win(post_artifact, loki_processed, margins.m_l2);
        assert!(
            processed.passed(),
            "a recorded §9.13 L2 run must clear the frozen processed M_L2: {processed:?}",
        );
        let storage = ourios_bench::bytes_must_win_tenths(post_artifact, loki_storage, tenths);
        assert!(
            storage.passed(),
            "a recorded §9.13 L2 run must clear the frozen storage floor: {storage:?}",
        );
    }

    // The pre-RFC-0033 total (2,549,129 B — the 513,862 B cold fold in
    // every query) fails the storage floor against run #16's Loki row
    // (1.05×): the frozen gate correctly refuses the configuration the
    // unfreeze condition existed to fix.
    let pre_artifact = ourios_bench::bytes_must_win_tenths(2_549_129, 2_673_545, tenths);
    assert!(!pre_artifact.passed(), "{pre_artifact:?}");
}

/// Scenario RFC0031.4 — L3 trace correlation wins on bytes read (OTLP-native).
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green under the §7 partial freeze (2026-07-13): `M_L3 = 10` on the
/// storage-side bytes channel. Locally provable without a container:
/// the frozen gate decides correctly on BOTH sides of the margin using
/// the §9.13-recorded measurements as documented historical evidence
/// (not a live measurement) — the pre-bloom run #12 configuration
/// (1.41×) correctly FAILS the frozen margin, and the post-bloom runs
/// #14–#17 (21.2–21.9×) clear it. The live assertion is
/// `rfc0031_indicative_comparative_run`.
#[test]
fn rfc0031_4_l3_trace_correlation_bytes() {
    let m_l3 = ourios_bench::ComparativeMargins::default().m_l3;
    assert_eq!(m_l3, 10, "the §7-frozen L3 must-win margin");

    // §9.13 run #12 — before the trace_id/span_id blooms Ourios fetched
    // the trace_id column corpus-wide (72,935,984 B): the frozen gate
    // correctly fails that configuration rather than flattering it.
    let pre_bloom = ourios_bench::bytes_must_win(72_935_984, 102_835_803, m_l3);
    assert!(!pre_bloom.passed(), "{pre_bloom:?}");

    // §9.13 runs #14–#17 — with blooms the fetch collapses to
    // 4,812,668 B and clears the frozen margin four runs in a row.
    for loki_storage in [105_353_837, 102_133_866, 104_656_570, 105_251_547] {
        let outcome = ourios_bench::bytes_must_win(4_812_668, loki_storage, m_l3);
        assert!(
            outcome.passed(),
            "a recorded §9.13 L3 run must clear the frozen M_L3: {outcome:?}",
        );
    }
}

/// Scenario RFC0031.5 — L4 frequency aggregation wins on bytes read (OTLP-native).
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// `M_L4` stays **§7-DEFERRED** ("deferred until L4 is first measured") —
/// unlike RFC0031.2/.4 (`M_L1`/`M_L3` frozen on the storage channel) or
/// RFC0031.7 (`F_L6` frozen on the latency channel), there is no recorded
/// measurement to pin a gate against yet, so this slice cannot pin a
/// frozen-value bound the way those scenarios do. What it pins instead —
/// the un-stub for this slice — is the **equivalence machinery** and the
/// **`PairSpec` shape** an L4 pair takes: the picker
/// ([`pick_frequency_pair`]), the Ourios aggregation extraction
/// (`ourios_bench::ourios_aggregate_answer`), the Loki matrix parser
/// (`ourios_bench::parse_loki_matrix`, including its bucket-alignment
/// convention), and the `(bucket, group_key) -> count` equivalence check
/// (`ourios_bench::compare_aggregations`) — all wired correctly on the
/// deterministic fixture. No container: the Loki side is a HAND-BUILT
/// matrix response over the SAME grouped counts the Ourios side measured
/// (mirroring how `pick_template_pair_finds_a_validated_needle` validates
/// the L1 picker against the Ourios engine alone, with the live
/// cross-system round trip left to the dispatch run). The bytes figure is
/// PRINTED, never asserted, per the deferral.
#[test]
fn rfc0031_5_l4_frequency_aggregation_bytes() {
    // Two values ("10"/"11") of one template ("connection established to
    // peer <id>" — the same ≥10-char constant run
    // `pick_template_pair_finds_a_validated_needle` already validates),
    // spread over four one-second buckets so the picker's cardinality
    // (2..=50) and row-floor bounds are exercised, not merely satisfied
    // trivially.
    let records: Vec<(u64, &str)> = vec![
        (100_000_000, "connection established to peer 10"),
        (500_000_000, "connection established to peer 10"),
        (900_000_000, "connection established to peer 11"),
        (2_100_000_000, "connection established to peer 10"),
        (2_500_000_000, "connection established to peer 11"),
        (3_000_000_000, "connection established to peer 11"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
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

    let pair = pick_frequency_pair(bucket.path(), &tenant, now, window)
        .expect("the peer template's trailing id validates as an L4 candidate");
    assert_eq!(pair.param, 0, "the template's only wildcard");
    assert_eq!(
        pair.bucket_width, "1s",
        "the ~2.9s fixture span floors to the 1s minimum bucket width",
    );
    assert_eq!(pair.needle, "connection established to peer");
    assert_eq!(pair.capture_regex, "peer\\s+(?P<value>\\S+)");
    let distinct: std::collections::HashSet<&String> =
        pair.groups.keys().map(|k| &k.group_key).collect();
    assert_eq!(distinct.len(), 2, "exactly the two picked id values");
    assert_eq!(pair.groups.values().sum::<u64>(), 6, "all six rows counted");
    assert!(
        pair.bytes_read > 0,
        "the grouped-count scan reads real bytes from storage",
    );

    // The `PairSpec` shape the dispatch run would fill in with a live
    // Loki round trip (RFC 0031 §3.4: L4 is a must-win class) — pinned
    // here without wiring the container-based measurement loop, per this
    // slice's scope.
    let margins = ourios_bench::ComparativeMargins::default();
    let spec = l4_pair_spec(
        &pair,
        built.min_effective_time_unix_nano,
        built.max_effective_time_unix_nano,
        now,
        window,
        &margins,
    )
    .expect("the fixture's capture_regex carries no backtick");
    assert!(
        matches!(spec.class.gate(), GateKind::MustWin),
        "L4 is a must-win class (RFC 0031 §3.4), same disposition as L1/L2/L3",
    );
    assert_eq!(
        spec.margin, 10,
        "m_l4's current (§7-deferred) default value"
    );

    // The Ourios half, run for real through the exact `spec.dsl` (no
    // container needed) — and proved to match the picker's own
    // measurement.
    let ourios = ourios_aggregate_answer(bucket.path(), &tenant, &spec.dsl, spec.now, spec.window)
        .expect("l4 aggregate answer");
    assert_eq!(ourios.groups, pair.groups);

    // The Loki half: a HAND-BUILT matrix response over the SAME grouped
    // counts (no container — the live cross-system round trip is the
    // dispatch run's job), proving `parse_loki_matrix`'s bucket-alignment
    // convention (eval instant t = (k+1)*w, decoded bucket start = t - w)
    // decodes to EXACTLY the Ourios map.
    let width_ns = bucket_width_seconds(&pair.bucket_width)
        .checked_mul(1_000_000_000)
        .expect("bucket width fits u64 nanoseconds");
    let loki_json = synthetic_loki_matrix(&pair.groups, width_ns);
    let loki_groups =
        parse_loki_matrix(&loki_json, "value", width_ns).expect("parse synthetic loki matrix");

    let outcome = compare_aggregations(&ourios.groups, &loki_groups, 8);
    assert!(
        outcome.is_equal(),
        "RFC0031.5 — the two systems' grouped-count maps must be identical: {outcome:?}",
    );

    // The mismatch arm (RFC0031.1's L4 half): dropping one cell from the
    // Loki side must report Mismatch, not silently pass — mirrors
    // RFC0031.1's `loki_narrow` assertion for the line-returning classes.
    let mut short = loki_groups.clone();
    let any_key = short.keys().next().cloned().expect("non-empty map");
    short.remove(&any_key);
    assert!(
        !compare_aggregations(&ourios.groups, &short, 8).is_equal(),
        "a dropped aggregation cell must report Mismatch, not silently pass",
    );

    // PRINTED, never asserted — M_L4 stays §7-deferred until the dispatch
    // run's first live measurement.
    println!(
        "RFC0031.5 [{}] ourios bytes_read (grouped-count scan) = {} — M_L4 DEFERRED \
         (RFC 0031 §7): reported, not asserted, until first measured",
        spec.label, ourios.bytes_read,
    );
}

/// Scenario RFC0031.6 — L5 substring needle measured + published, loss permitted.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.6 stub — implemented in the L-gate + reporting green slice"]
fn rfc0031_6_l5_substring_needle_published() {
    todo!(
        "RFC0031.6 — a literal not captured by a template or a promoted \
         column (embedded in a param, nothing prunes it), RFC0031.1 \
         holding: both systems' bytes_read + latency recorded, \
         disposition 'acknowledged'. Run PASSES regardless of winner — \
         an Ourios loss does not fail the run and does not escalate, but \
         MUST appear in the benchmarks.md §9 table (a suppressed L5 loss \
         is a process violation)"
    );
}

/// Scenario RFC0031.7 — L6 broad scan stays within the floor.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green under the §7 partial freeze (2026-07-13): `F_L6 = 3` FROZEN on
/// the LATENCY channel, exactly as the scenario is written
/// (`ourios.latency_p50 ≤ F_L6 × loki.latency_p50`); the window pairs'
/// bytes figures are reclassified to a published diagnostic (their
/// publication is RFC0031.11's ground). Locally provable: the floor
/// semantics at the frozen factor decide exactly at the boundary, and
/// the §9.13 run #18 window-pair p50s (documented historical evidence,
/// not a live measurement) all hold the floor. The live assertion —
/// loudly non-evaluable when latency is unmeasured, never a silent
/// pass or a spurious fail — is `rfc0031_indicative_comparative_run`.
#[test]
fn rfc0031_7_l6_broad_scan_floor() {
    let f_l6 = ourios_bench::ComparativeMargins::default().f_l6;
    assert_eq!(f_l6, 3, "the §7-frozen L6 latency floor factor");

    // The floor rule at the frozen factor: pass at ourios == 3 × loki,
    // fail one microsecond above — a Decided fail (the tuning-RFC
    // signal, not a pillar-level escalation), never an Invalid.
    let us = Duration::from_micros;
    assert!(latency_floor_gate(us(300), us(100), f_l6).passed());
    let over = latency_floor_gate(us(301), us(100), f_l6);
    assert!(!over.passed(), "{over:?}");
    assert!(matches!(
        over,
        ourios_bench::BytesGateOutcome::Decided { pass: false, .. }
    ));

    // §9.13 run #18 (the program's first latency measurements): all
    // three window pairs hold the frozen floor — 40.2 vs 13.8 ms,
    // 85.9 vs 294.8 ms, 38.8 vs 51.2 ms.
    for (ourios_p50, loki_p50) in [(40_200, 13_800), (85_900, 294_800), (38_800, 51_200)] {
        let outcome = latency_floor_gate(us(ourios_p50), us(loki_p50), f_l6);
        assert!(
            outcome.passed(),
            "a recorded §9.13 run-#18 window pair must hold the frozen F_L6: {outcome:?}",
        );
    }
}

/// Scenario RFC0031.8 — L7 ingest throughput parity within a stated factor.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.8 stub — implemented in the ingest-parity green slice"]
fn rfc0031_8_l7_ingest_throughput_parity() {
    todo!(
        "RFC0031.8 — OTLP replay driver feeding both systems to steady \
         state on the same hardware: ourios.ingest_throughput >= \
         loki.ingest_throughput / F_L7. The WAL-before-ack invariant \
         (CLAUDE.md §3.4) is NOT relaxed to obtain the number — Ourios \
         throughput is measured with durable acks and the config \
         proving it is recorded"
    );
}

/// Scenario RFC0031.9 — storage footprint is a diagnostic, not a gate.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.9 stub — implemented in the reporting + escalation green slice"]
fn rfc0031_9_storage_footprint_diagnostic() {
    todo!(
        "RFC0031.9 — both systems' persisted bytes on the shared bucket \
         and their ratio written to benchmarks.md §9 as a DIAGNOSTIC \
         row; no pass/fail derived from it (parity with A1's RFC 0011 \
         demotion)"
    );
}

/// Scenario RFC0031.10 — Loki config committed, competent, machine-checked.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.10 stub — implemented in the config-check green slice"]
fn rfc0031_10_loki_config_machine_checked() {
    todo!(
        "RFC0031.10 — the exact Loki config (index, chunk target size, \
         S3 backend, retention, frozen label set), the OTLP-into-Loki \
         config, and the DSL<->LogQL query pairs are present under \
         bench/comparative/ and the comparison runs with one documented \
         command; a test asserts the label set is drawn from a declared \
         low-cardinality allowlist and that trace_id, span_id, and any \
         per-template id are ABSENT (no catch-all-forcing-full-scan and \
         no high-cardinality label smuggling Ourios's columns into \
         Loki's index); each §9 row links the config commit"
    );
}

/// The repo root, resolved from the crate dir (the `rfc0024_calibration`
/// pattern) so the docs-presence scenario is cwd-independent.
fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Scenario RFC0031.11 — losses published and escalation follows §7.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
///
/// Green for the locally-testable half of the commitment: the published
/// record (`docs/benchmarks.md` §9.13) carries every class measured so
/// far — the L1/L3 wins, L2's honest short-of-margin disposition, AND
/// the window pairs' storage-channel loss figures (reclassified by the
/// §7 freeze from gated floor to published diagnostic — publishing them
/// honestly IS the commitment). A suppressed loss row is the process
/// violation the scenario forbids, so the loss figures' presence is
/// asserted against the committed doc. The escalation POLICY (an L1–L4
/// bytes loss is pillar-level; a latency-only loss with a bytes win is
/// a roadmap item) is prose in `benchmarks.md` §7 and the RFC; its
/// enforcement teeth are `rfc0031_indicative_comparative_run`'s
/// frozen-gate assertions.
#[test]
fn rfc0031_11_losses_published_and_escalation() {
    let text = std::fs::read_to_string(repo_root().join("docs/benchmarks.md"))
        .expect("read docs/benchmarks.md");
    let start = text
        .find("### 9.13 ")
        .expect("benchmarks.md carries the §9.13 comparative calibration record");
    let rest = &text[start..];
    let section = rest[1..].find("\n### ").map_or(rest, |i| &rest[..=i]);

    for (marker, why) in [
        // Unformatted substrings: a bold-only doc edit must not fail
        // this; deleting the figure itself must.
        ("77.2×", "the L1 flagship win (run #15) — wins publish too"),
        ("21.9×", "the L3 win (runs #14/#17)"),
        (
            "not a provisional 10× pass",
            "L2's honest short-of-margin disposition",
        ),
        (
            "Time-window browses (L6 floor family)",
            "the window-browse class heading",
        ),
        ("0.007 fail", "the k=2000 storage-channel loss row (run #8)"),
        (
            "0.016 fail",
            "the k=2000 storage-channel loss on current code (run #10)",
        ),
        (
            "0.018 fail",
            "the selective-resource diagnostic's storage loss (run #17)",
        ),
    ] {
        assert!(
            section.contains(marker),
            "§9.13 must publish {why} ({marker:?}) — deleting a loss figure \
             is the process violation RFC0031.11 forbids",
        );
    }
}

// ---------------------------------------------------------------------------
// Indicative comparative run (§7 calibration input) — dispatch-only.
// ---------------------------------------------------------------------------

/// The dynamically-picked first query pair: a `(service, severity
/// threshold, severity text)` whose rows form a small, exactly-equivalent
/// result set on both systems.
#[derive(Debug)]
struct SelectivePair {
    service: String,
    /// The DSL threshold: the pair selects rows with
    /// `severity_number ≥ threshold`.
    threshold: i32,
    /// The single `severity_text` those rows all carry (the `LogQL` side of
    /// the pair) — the picker's text-consistency guarantee.
    text: String,
    /// How many rows the pair selects.
    rows: u64,
    /// Corpus record count (for the report).
    total_records: u64,
    /// Corpus `time_unix_nano` span (the Loki query window).
    min_ts: u64,
    max_ts: u64,
}

/// Scan an OTLP/JSON Lines corpus and pick the query pair for the
/// indicative run: a `(service, threshold T, text t)` where **every** row
/// of the service with `severity_number ≥ T` carries the single
/// `severity_text == t`, and their count is `1..=4000` (under Loki's
/// 5000-line query cap, so the complete result fits one page). The
/// consistency requirement makes DSL `severity >= T` and `LogQL`
/// `severity_text="t"` express the same question, so the equivalence
/// check is meaningful. Zero-`time_unix_nano` rows are tallied
/// separately as POISON bands and disqualify any candidate whose
/// predicate could select them (see [`select_pair_candidates`]).
/// Generalised from a hardcoded ERROR band because
/// real captures vary — otel-demo v8 carries no ERROR logs at all (its
/// failure flags surface in traces/metrics), only INFO/Information and
/// four WARNs. Candidate thresholds are the service's **observed**
/// severity numbers — which is complete, because a gap threshold (say 16
/// when only 17 occurs) selects exactly the same rows as the next
/// observed number above it, adding no new candidates. Picks the FEWEST
/// rows; ties break to the lowest threshold then the lexicographically
/// smallest service, for deterministic reruns.
fn pick_selective_pair(corpus_dir: &std::path::Path) -> SelectivePair {
    use std::collections::HashMap;
    use std::io::BufRead as _;

    // service -> (clean bands, poison bands)
    let mut per_service: HashMap<String, (SeverityBands, SeverityBands)> = HashMap::new();
    let (mut total, mut min_ts, mut max_ts) = (0u64, u64::MAX, 0u64);

    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let service = rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .and_then(|v| match v {
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            s,
                        ) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                // One entry lookup per ResourceLogs group (moving the
                // service string in), not one clone per record — the scan
                // walks multi-million-record corpora. Severity texts are
                // likewise cloned only on their FIRST occurrence per
                // (service, number): get_mut hits the existing key for the
                // millions of repeats.
                let entry = per_service.entry(service).or_default();
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        total += 1;
                        // Zero-`time_unix_nano` records go into POISON
                        // bands, not candidate bands: both systems could
                        // still return them (Ourios windows the RFC 0005
                        // §3.2 EFFECTIVE timestamp, falling back to
                        // observed; Loki's OTLP ingest falls back to
                        // observed too), but with DIFFERENT answer
                        // timestamps — Ourios's row keeps time_unix_nano
                        // = 0, Loki stamps its stored (observed) time —
                        // so their LineKeys can never match and any
                        // candidate whose predicate could select such a
                        // row is a guaranteed equivalence mismatch.
                        let bands = if lr.time_unix_nano == 0 {
                            &mut entry.1
                        } else {
                            min_ts = min_ts.min(lr.time_unix_nano);
                            max_ts = max_ts.max(lr.time_unix_nano);
                            &mut entry.0
                        };
                        let texts = bands.entry(lr.severity_number).or_default();
                        if let Some(count) = texts.get_mut(lr.severity_text.as_str()) {
                            *count += 1;
                        } else {
                            texts.insert(lr.severity_text.clone(), 1);
                        }
                    }
                }
            }
        }
    }
    assert!(
        min_ts <= max_ts,
        "corpus has no record with a non-zero time_unix_nano — no query window derivable",
    );

    let mut candidates = select_pair_candidates(&per_service);
    candidates.sort();
    let Some(&(rows, threshold, service, text)) = candidates.first() else {
        panic!(
            "no (service, severity-threshold) with a single text and 1..=4000 rows; \
             per-service severity bands: {per_service:#?}"
        );
    };
    SelectivePair {
        service: service.clone(),
        threshold,
        text: text.clone(),
        rows,
        total_records: total,
        min_ts,
        max_ts,
    }
}

/// The corpus's `*.jsonl` files, sorted for deterministic scans.
fn corpus_jsonl_paths(corpus_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(corpus_dir)
        .expect("read corpus dir")
        .filter_map(|e| {
            let p = e.expect("dir entry").path();
            (p.extension().and_then(|x| x.to_str()) == Some("jsonl")).then_some(p)
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no *.jsonl in {}", corpus_dir.display());
    paths
}

/// Second streaming pass over the corpus, for the selectivity-curve
/// window pairs: every log record of `service`, split into CLEAN
/// timestamps (non-zero `time_unix_nano` — rows both systems answer
/// with identical keys) and POISON timestamps (the observed-fallback
/// effective time of zero-`time_unix_nano` rows: both systems would
/// RETURN such a row if a window covered its fallback time, but with
/// DIFFERENT answer timestamps — Ourios keeps `time_unix_nano = 0`,
/// Loki stamps its stored observed time — so any window containing one
/// is a guaranteed equivalence mismatch). Both sorted ascending.
fn collect_service_timestamps(corpus_dir: &std::path::Path, service: &str) -> (Vec<u64>, Vec<u64>) {
    use std::io::BufRead as _;

    let (mut clean, mut poison) = (Vec::new(), Vec::new());
    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let matches_service = rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .is_some_and(|v| {
                        matches!(
                            v,
                            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s)
                                if s == service
                        )
                    });
                if !matches_service {
                    continue;
                }
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        if lr.time_unix_nano != 0 {
                            clean.push(lr.time_unix_nano);
                        } else if lr.observed_time_unix_nano != 0 {
                            poison.push(lr.observed_time_unix_nano);
                        }
                    }
                }
            }
        }
    }
    clean.sort_unstable();
    poison.sort_unstable();
    (clean, poison)
}

/// Per-trace tally for [`pick_trace_pair`]. Allocation-free by design:
/// v8-scale corpora carry millions of mostly-singleton traces, so the
/// per-entry state holds an interned service id, not a `String`.
#[derive(Default)]
struct TraceTally {
    rows: u64,
    has_zero_ts: bool,
    has_empty_service: bool,
    first_service: Option<u32>,
    multi_service: bool,
}

/// Third streaming pass, for the L3 (trace-correlation) pair: tally
/// every 16-byte `trace_id` in the corpus and pick the one to query.
/// Eligibility mirrors the other pickers' honesty rules: no
/// zero-`time_unix_nano` row (the two systems would answer with
/// different keys — a guaranteed mismatch) and no empty-service row
/// (the `LogQL` side's `{service_name=~".+"}` selector could not see it),
/// with `2..=100` rows so the result is a meaningful multiset that fits
/// one page. Preference order: multi-service traces first (the class's
/// structural point — Loki cannot pre-narrow a cross-service trace to
/// one stream), then the most rows, then the lexicographically smallest
/// id for deterministic reruns. Returns `(hex_id, rows)`.
fn pick_trace_pair(corpus_dir: &std::path::Path) -> Option<(String, u64)> {
    use std::collections::HashMap;
    use std::io::BufRead as _;

    let mut traces: HashMap<[u8; 16], TraceTally> = HashMap::new();
    // Service names interned to u32 ids — one small map over the handful
    // of distinct services, zero per-trace allocations.
    let mut service_ids: HashMap<String, u32> = HashMap::new();
    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let service = rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .and_then(|v| match v {
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            s,
                        ) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                // get-then-insert (not entry) so the millions of repeat
                // ResourceLogs never allocate a lookup key.
                let service_id = if let Some(&id) = service_ids.get(service) {
                    id
                } else {
                    let id = u32::try_from(service_ids.len()).expect("service count fits u32");
                    service_ids.insert(service.to_string(), id);
                    id
                };
                let service_is_empty = service.is_empty();
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        let Ok(id) = <[u8; 16]>::try_from(lr.trace_id.as_slice()) else {
                            continue;
                        };
                        let tally = traces.entry(id).or_default();
                        tally.rows += 1;
                        tally.has_zero_ts |= lr.time_unix_nano == 0;
                        tally.has_empty_service |= service_is_empty;
                        match tally.first_service {
                            None => tally.first_service = Some(service_id),
                            Some(first) if first != service_id => tally.multi_service = true,
                            Some(_) => {}
                        }
                    }
                }
            }
        }
    }
    traces
        .into_iter()
        .filter(|(_, t)| !t.has_zero_ts && !t.has_empty_service && (2..=100).contains(&t.rows))
        .max_by(|(a_id, a), (b_id, b)| {
            (a.multi_service, a.rows, std::cmp::Reverse(a_id)).cmp(&(
                b.multi_service,
                b.rows,
                std::cmp::Reverse(b_id),
            ))
        })
        .map(|(id, t)| {
            use std::fmt::Write as _;
            let hex = id.iter().fold(String::new(), |mut out, b| {
                let _ = write!(out, "{b:02x}");
                out
            });
            (hex, t.rows)
        })
}

/// The dynamically-picked L1 (template-exact lookup) pair: DSL
/// `template_id == N` — riding the writer's existing bloom filter on
/// `template_id` — against `LogQL` `{service_name=~".+"} |= "<needle>"`.
/// Loki has no template concept, so its honest equivalent is a line
/// filter over every stream.
#[derive(Debug)]
struct TemplatePair {
    template_id: u64,
    /// The template's constant text: contained in every one of the
    /// template's rows (bit-identical reconstruction, `CLAUDE.md` §3.3)
    /// and — validated against the corpus — in NO other line, so the two
    /// queries select identical row sets.
    needle: String,
    /// How many rows the pair selects (the validated count).
    rows: u64,
}

/// A template's candidate needle: the longest run of consecutive
/// [`OwnedToken::Fixed`] tokens joined with single spaces, kept only at
/// ≥ 10 chars. Runs split at wildcards AND at tokens outside the safe
/// charset ([`select_pair_candidates`]' rule — the needle lands inside a
/// quoted `LogQL` string literal). Separators are per-ROW state, not
/// template state, so the single-space join is an assumption about the
/// common case; [`pick_template_pair`]'s containment validation rejects
/// any candidate it fails for. Length ties break to the
/// lexicographically smallest run, for deterministic reruns.
fn template_needle(tokens: &[OwnedToken]) -> Option<String> {
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ' '))
    };
    let mut runs: Vec<String> = Vec::new();
    let mut run: Vec<&str> = Vec::new();
    for token in tokens {
        match token {
            OwnedToken::Fixed(s) if safe(s) => run.push(s),
            _ => {
                if !run.is_empty() {
                    runs.push(run.join(" "));
                    run.clear();
                }
            }
        }
    }
    if !run.is_empty() {
        runs.push(run.join(" "));
    }
    runs.retain(|needle| needle.len() >= 10);
    runs.into_iter().min_by(|a, b| {
        (std::cmp::Reverse(a.len()), a.as_str()).cmp(&(std::cmp::Reverse(b.len()), b.as_str()))
    })
}

/// Per-needle tally from [`tally_needles`]' corpus pass.
#[derive(Default)]
struct NeedleTally {
    /// Corpus lines whose string body contains the needle.
    matches: u64,
    /// A matching line has `time_unix_nano == 0` — the established
    /// key-mismatch poison rule (the two systems answer with different
    /// keys).
    has_zero_ts: bool,
    /// A matching line has no `service.name` — invisible to the `LogQL`
    /// side's `{service_name=~".+"}` selector.
    has_empty_service: bool,
}

/// One streaming corpus pass for the L1 picker: for each candidate
/// needle, count the corpus lines whose string body CONTAINS it, and
/// record whether any matching line is poisoned. Non-string bodies never
/// match (a needle can only select the string-body lines whose bytes
/// both systems return identically).
fn tally_needles(corpus_dir: &std::path::Path, candidates: &[(u64, String)]) -> Vec<NeedleTally> {
    use std::io::BufRead as _;

    let mut tallies: Vec<NeedleTally> = candidates.iter().map(|_| NeedleTally::default()).collect();
    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let service_is_empty = !rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .is_some_and(|v| {
                        matches!(
                            v,
                            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s)
                                if !s.is_empty()
                        )
                    });
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        let Some(
                            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                                body,
                            ),
                        ) = lr.body.as_ref().and_then(|b| b.value.as_ref())
                        else {
                            continue;
                        };
                        for ((_, needle), tally) in candidates.iter().zip(&mut tallies) {
                            if body.contains(needle.as_str()) {
                                tally.matches += 1;
                                tally.has_zero_ts |= lr.time_unix_nano == 0;
                                tally.has_empty_service |= service_is_empty;
                            }
                        }
                    }
                }
            }
        }
    }
    tallies
}

/// The L1 candidates worth validating, ordered most-selective first:
/// needle-match count in `2..=4000` (a meaningful multiset that fits one
/// Loki page) and no poisoned matching line. Sorted by fewest matches,
/// then the longest needle, then the lexicographically smallest needle,
/// then the smallest template id — deterministic reruns.
fn eligible_template_candidates<'a>(
    candidates: &'a [(u64, String)],
    tallies: &[NeedleTally],
) -> Vec<(u64, &'a str, u64)> {
    let mut eligible: Vec<(u64, &str, u64)> = candidates
        .iter()
        .zip(tallies)
        .filter(|(_, t)| !t.has_zero_ts && !t.has_empty_service && (2..=4000).contains(&t.matches))
        .map(|((id, needle), t)| (*id, needle.as_str(), t.matches))
        .collect();
    eligible.sort_by(|a, b| {
        (a.2, std::cmp::Reverse(a.1.len()), a.1, a.0).cmp(&(
            b.2,
            std::cmp::Reverse(b.1.len()),
            b.1,
            b.0,
        ))
    });
    eligible
}

/// Fourth picker, for the L1 (template-exact lookup) pair — runs AFTER
/// `build_comparative_store`, because template ids exist only
/// post-mining. Derives the tenant's template registry via the querier
/// (RFC 0017 §3.2), takes each template's longest constant run as its
/// candidate needle ([`template_needle`]), and VALIDATES equivalence
/// before selection. A candidate survives iff:
///
/// - the count of corpus lines containing the needle equals the
///   template's row count (an Ourios count query, `template_id == N`,
///   no limit) — every template row contains its constant text
///   (bit-identical reconstruction, `CLAUDE.md` §3.3), so count-equality
///   proves the needle selects exactly the template's rows and nothing
///   else;
/// - every RENDERED template row actually contains the needle — the
///   containment direction count-equality alone cannot prove for a
///   multi-token needle (separators are per-row state; the single-space
///   join is only the common case);
/// - the shared honesty rules ([`eligible_template_candidates`]):
///   `2..=4000` rows, no zero-`time_unix_nano` and no empty-service line
///   among the needle matches.
///
/// Rejections are LOUD (stderr), and so is the no-valid-template skip
/// (the [`build_pair_specs`] `None` arm).
fn pick_template_pair(
    corpus_dir: &std::path::Path,
    bucket_root: &std::path::Path,
    tenant: &TenantId,
    now: u64,
    window: u64,
) -> Option<TemplatePair> {
    use std::collections::HashMap;
    use std::collections::hash_map::Entry;

    let querier = ourios_querier::Querier::new(bucket_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let registry = runtime
        .block_on(querier.template_registry(tenant))
        .expect("derive template registry");

    // Highest version per template id: widening only ever turns Fixed
    // positions into Wildcards (RFC 0001 §6.2 step 5), so the latest
    // version's constants are constant in every earlier version's rows
    // too — and the DSL `template_id == N` selects rows of EVERY version.
    let mut latest: HashMap<u64, (u32, Vec<OwnedToken>)> = HashMap::new();
    for ((id, version), tokens) in registry {
        match latest.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert((version, tokens));
            }
            Entry::Occupied(mut entry) => {
                if version > entry.get().0 {
                    entry.insert((version, tokens));
                }
            }
        }
    }
    let mut candidates: Vec<(u64, String)> = latest
        .into_iter()
        .filter(|&(id, _)| id != ourios_miner::cluster::NO_TEMPLATE)
        .filter_map(|(id, (_, tokens))| template_needle(&tokens).map(|needle| (id, needle)))
        .collect();
    // Deterministic tally/validation order regardless of map iteration.
    candidates.sort_unstable();

    let tallies = tally_needles(corpus_dir, &candidates);
    for (template_id, needle, matches) in eligible_template_candidates(&candidates, &tallies) {
        let dsl = format!("template_id == {template_id}");
        let query = ourios_querier::dsl::parse(&dsl).expect("L1 count DSL parses");
        let rows = runtime
            .block_on(querier.run_query(&query, tenant, now, window, None))
            .expect("L1 count query")
            .rows;
        if rows != matches {
            eprintln!(
                "L1 candidate template_id={template_id} needle={needle:?} rejected: \
                 {rows} template rows vs {matches} needle-matching corpus lines",
            );
            continue;
        }
        match ourios_bench::ourios_query_answer(
            bucket_root,
            tenant,
            &format!("{dsl} | limit 5000"),
            now,
            window,
        ) {
            Ok(answer) => {
                if answer.lines.iter().all(|line| {
                    line.body
                        .windows(needle.len())
                        .any(|w| w == needle.as_bytes())
                }) {
                    return Some(TemplatePair {
                        template_id,
                        needle: needle.to_string(),
                        rows,
                    });
                }
                eprintln!(
                    "L1 candidate template_id={template_id} needle={needle:?} rejected: a \
                     rendered row does not contain the needle (per-row separators broke \
                     the single-space join)",
                );
            }
            Err(e) => eprintln!(
                "L1 candidate template_id={template_id} needle={needle:?} rejected: row \
                 materialization failed ({e:?})",
            ),
        }
    }
    None
}

// ---------------------------------------------------------------------------
// L4 (frequency-aggregation) pair — the fourth post-store-build picker.
// ---------------------------------------------------------------------------

/// The lowest floor on total matching rows a [`pick_frequency_pair`]
/// candidate must clear — enough for the grouped-count map to be a
/// meaningful multi-cell answer, not a trivially-passing edge case.
const L4_MIN_ROWS: u64 = 4;

/// The highest ceiling on total matching rows a [`pick_frequency_pair`]
/// candidate may have — a smaller candidate proves the same
/// `(bucket, group_key) → count` equivalence just as validly as a
/// larger one, and [`loki_measure_frequency_pair`]'s poll shares the
/// same 300 s deadline every other class's [`loki_measure_pair`] uses.
/// A ~971K-row candidate (a service's dominant, near-catch-all
/// template) genuinely could not finish ingesting/processing within
/// that window on a real dispatch — 100K rows leaves comfortable
/// margin at the observed ~2.7K rows/s Loki throughput (~37 s of the
/// 300 s budget) while still being a large, meaningful multiset.
const L4_MAX_ROWS: u64 = 100_000;

/// The distinct-param-value cardinality band a [`pick_frequency_pair`]
/// candidate must fall within: `2` (a single value is not a grouping
/// question) through `50` (moderate cardinality — a low-cardinality
/// `GROUP BY` an operator would actually run, per the RFC 0031 §2.3 L4
/// motivation; this is also what naturally rejects a per-line-unique slot
/// like kafka's, without a special case for it).
const L4_CARDINALITY: std::ops::RangeInclusive<usize> = 2..=50;

/// Choose a `bucket(width)` for the L4 pair from a query's time span: the
/// largest whole DSL duration unit (`w`/`d`/`h`/`m`/`s`) that divides the
/// span into roughly [`L4_TARGET_BUCKETS`] windows, floored at the DSL's
/// finest unit (`1s`) so a short capture window still splits into more
/// than one bucket. RFC 0031 §7 leaves "which bucket width" open pending
/// v8-scale tuning at dispatch time; this is a documented, deterministic
/// default, not a frozen value.
fn pick_bucket_width(span_ns: u64) -> String {
    const L4_TARGET_BUCKETS: u64 = 4;
    const MIN_BUCKET_NS: u64 = 1_000_000_000;
    const UNITS: [(&str, u64); 5] = [
        ("w", 7 * 86_400 * 1_000_000_000),
        ("d", 86_400 * 1_000_000_000),
        ("h", 3_600 * 1_000_000_000),
        ("m", 60 * 1_000_000_000),
        ("s", 1_000_000_000),
    ];
    let target = (span_ns / L4_TARGET_BUCKETS).max(MIN_BUCKET_NS);
    for (suffix, unit_ns) in UNITS {
        if target >= unit_ns {
            return format!("{}{suffix}", target / unit_ns);
        }
    }
    unreachable!("target is floored at MIN_BUCKET_NS, the \"s\" unit's own width")
}

/// Convert a `bucket(width)` lexeme this harness emits (`pick_bucket_width`'s
/// output — `s`/`m`/`h`/`d`/`w`, never sub-second) to whole seconds, the
/// unit Loki's `query_range` `start`/`end`/`step` parameters take.
fn bucket_width_seconds(width: &str) -> u64 {
    let (digits, unit) = width.split_at(width.len() - 1);
    let n: u64 = digits
        .parse()
        .unwrap_or_else(|e| panic!("bucket width {width:?} digits: {e}"));
    let per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        other => panic!("bucket width {width:?} has unknown unit {other:?}"),
    };
    n * per_unit
}

/// Escape Go RE2 metacharacters (Loki's `regexp` stage dialect) in a
/// literal template constant.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '^' | '$' | '|' | '(' | ')' | '[' | ']' | '{' | '}' | '*' | '+' | '?' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build a Loki `regexp` pattern (Go RE2 syntax) capturing one template
/// parameter as the named group `value` — the RFC 0031 §5 L4 "`LogQL`
/// pattern/regexp extraction" (§2.3). Anchors on the wildcard's immediate
/// fixed-token neighbours, not the whole line: a narrower pattern is more
/// robust to separator variance elsewhere in the line (per-row separators
/// are not template state — the same known limitation [`template_needle`]
/// documents) and unambiguous whenever at least one neighbour is fixed.
/// Returns `None` if `target_param` is not a wildcard position in
/// `tokens`, **or** if neither neighbour is a fixed token — an
/// unanchored `(?P<value>\S+)` would match any token and silently feed
/// wrong equivalence input to `compare_aggregations`, so the caller must
/// reject the candidate instead of measuring it.
fn param_capture_regex(tokens: &[OwnedToken], target_param: u32) -> Option<String> {
    let mut wildcard_idx = 0u32;
    let pos = tokens.iter().position(|t| {
        if matches!(t, OwnedToken::Wildcard) {
            let is_target = wildcard_idx == target_param;
            wildcard_idx += 1;
            is_target
        } else {
            false
        }
    })?;
    let fixed_at = |i: usize| match tokens.get(i) {
        Some(OwnedToken::Fixed(s)) => Some(s.as_str()),
        _ => None,
    };
    let prev = pos.checked_sub(1).and_then(fixed_at);
    let next = fixed_at(pos + 1);
    if prev.is_none() && next.is_none() {
        return None;
    }
    let mut pattern = String::new();
    if let Some(prev) = prev {
        pattern.push_str(&regex_escape(prev));
        pattern.push_str(r"\s+");
    }
    pattern.push_str(r"(?P<value>\S+)");
    if let Some(next) = next {
        pattern.push_str(r"\s+");
        pattern.push_str(&regex_escape(next));
    }
    Some(pattern)
}

/// The dynamically-picked L4 (frequency-aggregation) pair: a
/// `(template_id, param position, bucket width)` whose grouped-count map
/// is a moderate-cardinality, multi-row answer.
#[derive(Debug)]
struct FrequencyPair {
    template_id: u64,
    /// The wildcard position within the template extracted as the group
    /// key (`param(n)`, 0-based over wildcards only).
    param: u32,
    /// The template's constant needle for the Loki `|=` stream-selector
    /// pre-filter ([`template_needle`] — the same machinery the L1 pair
    /// uses).
    needle: String,
    /// The Loki `regexp` pattern extracting `param` as the named group
    /// `value` ([`param_capture_regex`]).
    capture_regex: String,
    /// The `bucket(width)` lexeme ([`pick_bucket_width`]).
    bucket_width: String,
    /// The validated grouped-count map (from the real Ourios aggregate
    /// query — the picker's own selection criterion).
    groups: HashMap<AggKey, u64>,
    /// The grouped-count scan's bytes read (RFC 0031 §3.6).
    bytes_read: u64,
}

/// Fourth picker (post-store-build, like [`pick_template_pair`]): a
/// per-template, per-param-slot cardinality tally over the corpus,
/// choosing the first `(template_id, param position)` whose grouped-count
/// map (at a bucket width derived from the query's own time span,
/// [`pick_bucket_width`]) has [`L4_CARDINALITY`] distinct group values and
/// at least [`L4_MIN_ROWS`] total matching rows — moderate cardinality,
/// enough rows to be a meaningful multiset, and (naturally, via the
/// cardinality bound) never a per-line-unique slot like kafka's. Iterates
/// templates by ascending id and, within a template, param slots by
/// ascending index — a deterministic pick. Every rejected candidate logs
/// its reason to stderr (the run-#16 "no silent fallthrough" lesson); a
/// candidate that fails the SAME query bounds check but has no usable
/// needle or capture regex is rejected too, loudly.
/// Whether an aggregate candidate's grouped-count map meets the L4
/// picker's shape floors/ceiling — moderate cardinality
/// ([`L4_CARDINALITY`]), a row-count band (at least [`L4_MIN_ROWS`],
/// at most [`L4_MAX_ROWS`] — too many rows makes
/// [`loki_measure_frequency_pair`]'s poll unable to finish within its
/// shared 300 s deadline before Loki has ingested/processed them
/// all), and at least 2 distinct bucket starts (otherwise the
/// candidate never exercises the bucket dimension of the `(bucket,
/// group_key) → count` equivalence shape, leaving
/// [`parse_loki_matrix`]'s bucket-alignment convention untested on
/// this corpus). Returns the rejection reason, or `None` if the
/// candidate passes every bound.
fn frequency_shape_rejection(groups: &HashMap<AggKey, u64>) -> Option<String> {
    let distinct_values: std::collections::HashSet<&String> =
        groups.keys().map(|k| &k.group_key).collect();
    if !L4_CARDINALITY.contains(&distinct_values.len()) {
        return Some(format!(
            "{} distinct param values (need {L4_CARDINALITY:?})",
            distinct_values.len(),
        ));
    }
    let total_rows: u64 = groups.values().sum();
    if total_rows < L4_MIN_ROWS {
        return Some(format!("{total_rows} rows (need >= {L4_MIN_ROWS})"));
    }
    if total_rows > L4_MAX_ROWS {
        return Some(format!(
            "{total_rows} rows (need <= {L4_MAX_ROWS}, so the Loki poll can finish within \
             its shared 300s deadline)"
        ));
    }
    let distinct_buckets: std::collections::HashSet<u64> =
        groups.keys().map(|k| k.bucket_start_unix_nanos).collect();
    if distinct_buckets.len() < 2 {
        return Some(format!(
            "all rows land in {} bucket window(s) (need >= 2, to exercise the bucket dimension)",
            distinct_buckets.len(),
        ));
    }
    None
}

/// Build the L4 `PairSpec` from a picked [`FrequencyPair`]. The `regexp`
/// argument is backtick-delimited (a `LogQL`/Go raw string literal): the
/// pattern already carries Go RE2 escapes (`\s+`, `\S+`, and any
/// `regex_escape`d metacharacter) from [`param_capture_regex`], and a
/// double-quoted `LogQL` string literal would try to interpret those same
/// backslashes as *its own* escape sequences — `\s` is not a valid one,
/// so Loki's parser rejects the query with "invalid char escape" before
/// the pattern ever reaches the regex engine. Returns `None` (loudly
/// logged) if `capture_regex` itself contains a backtick — the fixed
/// corpus token would prematurely close the raw string, and `regex_escape`
/// does not escape backticks (they are not an RE2 metacharacter).
fn l4_pair_spec(
    pair: &FrequencyPair,
    min_effective_time_unix_nano: u64,
    max_effective_time_unix_nano: u64,
    now: u64,
    window: u64,
    margins: &ourios_bench::ComparativeMargins,
) -> Option<PairSpec> {
    if pair.capture_regex.contains('`') {
        eprintln!(
            "L4 pair template_id={} param({}): rejected — capture_regex contains a \
             backtick, which would break the LogQL raw-string delimiter: {:?}",
            pair.template_id, pair.param, pair.capture_regex,
        );
        return None;
    }
    Some(PairSpec {
        label: format!(
            "frequency aggregation, L4 family: template_id={} param({}) bucket({})",
            pair.template_id, pair.param, pair.bucket_width,
        ),
        margin: margins.m_l4,
        class: PairClass::L4,
        dsl: format!(
            "template_id == {} | count by param({}), bucket({})",
            pair.template_id, pair.param, pair.bucket_width,
        ),
        logql: format!(
            "sum by (value) (count_over_time({{service_name=~\".+\"}} |= \"{}\" \
             | regexp `{}` [{}]))",
            pair.needle, pair.capture_regex, pair.bucket_width,
        ),
        start: min_effective_time_unix_nano,
        end: max_effective_time_unix_nano
            .checked_add(1)
            .expect("corpus max timestamp overflows the Loki window end"),
        expected_rows: pair.groups.values().sum(),
        now,
        window,
    })
}

fn pick_frequency_pair(
    bucket_root: &std::path::Path,
    tenant: &TenantId,
    now: u64,
    window: u64,
) -> Option<FrequencyPair> {
    use std::collections::HashMap as StdHashMap;
    use std::collections::hash_map::Entry;

    let querier = ourios_querier::Querier::new(bucket_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let registry = runtime
        .block_on(querier.template_registry(tenant))
        .expect("derive template registry");

    // Highest version per template id — mirrors `pick_template_pair`'s
    // rationale: widening only ever turns Fixed positions into Wildcards,
    // so the latest version's wildcard positions are wildcards in every
    // earlier version's rows too.
    let mut latest: StdHashMap<u64, (u32, Vec<OwnedToken>)> = StdHashMap::new();
    for ((id, version), tokens) in registry {
        match latest.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert((version, tokens));
            }
            Entry::Occupied(mut entry) => {
                if version > entry.get().0 {
                    entry.insert((version, tokens));
                }
            }
        }
    }
    let mut ids: Vec<u64> = latest
        .keys()
        .copied()
        .filter(|&id| id != ourios_miner::cluster::NO_TEMPLATE)
        .collect();
    ids.sort_unstable();

    let bucket_width = pick_bucket_width(window);

    for id in ids {
        let tokens = &latest[&id].1;
        let wildcard_count = tokens
            .iter()
            .filter(|t| matches!(t, OwnedToken::Wildcard))
            .count();
        for param in 0..u32::try_from(wildcard_count).expect("wildcard count fits u32") {
            let dsl =
                format!("template_id == {id} | count by param({param}), bucket({bucket_width})");
            let answer = match ourios_aggregate_answer(bucket_root, tenant, &dsl, now, window) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!(
                        "L4 candidate template_id={id} param({param}): aggregate query \
                         failed: {e:?}",
                    );
                    continue;
                }
            };
            if let Some(reason) = frequency_shape_rejection(&answer.groups) {
                eprintln!("L4 candidate template_id={id} param({param}): rejected — {reason}");
                continue;
            }
            let Some(needle) = template_needle(tokens) else {
                eprintln!(
                    "L4 candidate template_id={id} param({param}): rejected — no ≥10-char \
                     constant needle for the Loki stream-selector filter",
                );
                continue;
            };
            let Some(capture_regex) = param_capture_regex(tokens, param) else {
                eprintln!(
                    "L4 candidate template_id={id} param({param}): rejected — no fixed \
                     neighbour token to anchor the Loki capture regex on (an unanchored \
                     pattern would match any token)",
                );
                continue;
            };
            return Some(FrequencyPair {
                template_id: id,
                param,
                needle,
                capture_regex,
                bucket_width,
                groups: answer.groups,
                bytes_read: answer.bytes_read,
            });
        }
    }
    None
}

/// Render a synthetic Loki matrix `query_range` response over the SAME
/// grouped counts an [`ourios_aggregate_answer`] call measured — the
/// no-container proof that [`parse_loki_matrix`]'s bucket-alignment
/// convention (evaluation instant `t = (k+1)*width`, decoded bucket start
/// `t - width`) round-trips exactly. Samples are grouped by
/// [`AggKey::group_key`] under the `value` metric label and sorted by
/// timestamp ascending within each series (cosmetic — `parse_loki_matrix`
/// does not require sample order).
fn synthetic_loki_matrix(groups: &HashMap<AggKey, u64>, bucket_width_ns: u64) -> String {
    let mut by_label: std::collections::BTreeMap<String, Vec<(u64, u64)>> =
        std::collections::BTreeMap::new();
    for (key, &count) in groups {
        let t_ns = key
            .bucket_start_unix_nanos
            .checked_add(bucket_width_ns)
            .expect("bucket end fits u64 nanoseconds");
        by_label
            .entry(key.group_key.clone())
            .or_default()
            .push((t_ns, count));
    }
    let result: Vec<serde_json::Value> = by_label
        .into_iter()
        .map(|(label, mut samples)| {
            samples.sort_unstable_by_key(|&(t, _)| t);
            let values: Vec<serde_json::Value> = samples
                .into_iter()
                .map(|(t_ns, count)| {
                    let seconds = t_ns / 1_000_000_000;
                    serde_json::json!([seconds, count.to_string()])
                })
                .collect();
            serde_json::json!({ "metric": { "value": label }, "values": values })
        })
        .collect();
    serde_json::json!({
        "status": "success",
        "data": { "resultType": "matrix", "result": result },
    })
    .to_string()
}

/// The selective-resource diagnostic's service pick: the corpus's
/// LOWEST-volume safe-named service that still yields a clean window
/// (maintainer's positioning point, 2026-07-12: an enriched
/// resource-scoped browse should prune via the promoted `service.name`
/// bloom — the measured L6 windows used the HIGHEST-volume service,
/// which is present in every row group and is the bloom's worst case
/// by construction, so they bound the UNSCOPED-browse cost, not the
/// enriched one). `exclude` is the service the L6 window pairs already
/// use: falling through to it would duplicate an existing measurement,
/// which is a vacuous diagnostic (run #16 measured exactly that before
/// this guard existed). One corpus pass collects every service's
/// timestamps at once — the per-service rescan cost run #16 ~70
/// minutes. Every skipped candidate logs WHY, so the run log records
/// the corpus's enrichment reality even when the pick falls through.
type ServiceTimestamps = (Vec<u64>, Vec<u64>);

fn pick_rare_window_pair(
    corpus_dir: &std::path::Path,
    exclude: &str,
) -> Option<(String, u64, u64, u64)> {
    use std::collections::HashMap;
    use std::io::BufRead as _;

    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ' '))
    };
    // service -> (clean ts, poison ts), both filled in ONE corpus pass.
    let mut per_service: HashMap<String, ServiceTimestamps> = HashMap::new();
    for path in corpus_jsonl_paths(corpus_dir) {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            for rl in &data.resource_logs {
                let service = rl
                    .resource
                    .as_ref()
                    .and_then(|r| r.attributes.iter().find(|kv| kv.key == "service.name"))
                    .and_then(|kv| kv.value.as_ref())
                    .and_then(|v| v.value.as_ref())
                    .and_then(|v| match v {
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            s,
                        ) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                if !safe(service) || service == exclude {
                    continue;
                }
                // get-then-insert (the file's established pattern) so
                // repeat ResourceLogs never allocate a lookup key.
                let entry = if let Some(entry) = per_service.get_mut(service) {
                    entry
                } else {
                    per_service.entry(service.to_string()).or_default()
                };
                for sl in &rl.scope_logs {
                    for lr in &sl.log_records {
                        if lr.time_unix_nano != 0 {
                            entry.0.push(lr.time_unix_nano);
                        } else if lr.observed_time_unix_nano != 0 {
                            entry.1.push(lr.observed_time_unix_nano);
                        }
                    }
                }
            }
        }
    }
    let mut by_volume: Vec<(String, ServiceTimestamps)> = per_service.into_iter().collect();
    by_volume.sort_by(|a, b| {
        let (av, bv) = (a.1.0.len() + a.1.1.len(), b.1.0.len() + b.1.1.len());
        av.cmp(&bv).then_with(|| a.0.cmp(&b.0))
    });
    for (service, (mut clean, mut poison)) in by_volume {
        if clean.len() < 2 {
            eprintln!(
                "selective-resource candidate {service}: skipped — {} clean timestamps \
                 ({} zero-time rows); a source-timestamp-quality gap in the corpus, \
                 not a picker gap",
                clean.len(),
                poison.len(),
            );
            continue;
        }
        clean.sort_unstable();
        poison.sort_unstable();
        // Descending window-size ladder — a deliberate SAMPLING of the
        // 2..=100 range, not an exhaustive search: the largest k may
        // have no clean edges even when smaller ones do, and a
        // lower-volume service should win at any sampled size before a
        // busier one is tried. A window valid only at an unsampled
        // intermediate k falls through — acceptable for a diagnostic,
        // where 99 validity passes per service buys no measurement
        // value. Clamped duplicates are skipped.
        let mut tried = 0usize;
        for k in [100usize, 50, 20, 10, 5, 2] {
            let k = k.min(clean.len());
            if k < 2 {
                break;
            }
            if k == tried {
                continue;
            }
            tried = k;
            if let Some((start, end)) = pick_window_pair(&clean, &poison, k) {
                return Some((service, start, end, k as u64));
            }
        }
        eprintln!(
            "selective-resource candidate {service}: skipped — {} clean timestamps but \
             no clean-edged window at any sampled k",
            clean.len(),
        );
    }
    None
}

/// Pick a `k`-row time-window slice `[a, b)` of a service's records with
/// CLEAN EDGES, preferring mid-corpus. `clean_ts` and `poison_ts` come
/// sorted from [`collect_service_timestamps`]. A candidate start index
/// `i` is valid iff:
///
/// - **start edge**: no earlier record shares `clean_ts[i]`, so `a =
///   clean_ts[i]` admits exactly the window's rows;
/// - **end edge**: the next record (if any) sits ≥ 2 ns past the last
///   in-window one, so `b = last + 1` selects the same rows whether a
///   system treats its range end as inclusive or exclusive;
/// - no poison timestamp falls in `[a, b)`.
///
/// The window then contains exactly `k` rows on both systems. Returns
/// `(a, b)`, or `None` if no valid window exists.
fn pick_window_pair(clean_ts: &[u64], poison_ts: &[u64], k: usize) -> Option<(u64, u64)> {
    if k == 0 || clean_ts.len() < k {
        return None;
    }
    let valid = |i: usize| {
        let (first, last) = (clean_ts[i], clean_ts[i + k - 1]);
        if i > 0 && clean_ts[i - 1] == first {
            return false;
        }
        let Some(b) = last.checked_add(1) else {
            return false;
        };
        if i + k < clean_ts.len() && clean_ts[i + k] <= b {
            return false;
        }
        let at = poison_ts.partition_point(|&t| t < first);
        poison_ts.get(at).is_none_or(|&t| t >= b)
    };
    let centre = (clean_ts.len() - k) / 2;
    let i = (0..=clean_ts.len() - k)
        .filter(|&i| valid(i))
        .min_by_key(|&i| (i.abs_diff(centre), i))?;
    Some((clean_ts[i], clean_ts[i + k - 1].checked_add(1)?))
}

/// `severity_number -> severity_text -> row count`, one map per service
/// for clean (non-zero-time) rows and one for poison (zero-time) rows.
type SeverityBands = std::collections::HashMap<i32, std::collections::HashMap<String, u64>>;

/// The candidate `(rows, threshold, service, text)` tuples for
/// [`pick_selective_pair`]: for every service and every DISTINCT severity
/// number T in its clean bands (the nested map dedupes them), the rows
/// with `number ≥ T` are a candidate iff (a) they all share ONE text t,
/// (b) the count is `1..=4000`, (c) — the reverse direction — NO row with
/// text t sits below T, so `LogQL`'s text filter selects exactly the same
/// rows as the DSL's number threshold, and (d) no POISON row could be
/// selected by EITHER side's predicate: none with `number ≥ T` (the DSL
/// side) and none carrying text t at any number (the `LogQL` side) —
/// the severity pair queries the full corpus window, so a selectable
/// zero-time row is a guaranteed equivalence mismatch. No-service
/// (empty-key) records are scanned — they show in the failure diagnostic
/// — but never form candidates (an empty service can't make a valid
/// DSL/LogQL pair).
fn select_pair_candidates(
    per_service: &std::collections::HashMap<String, (SeverityBands, SeverityBands)>,
) -> Vec<(u64, i32, &String, &String)> {
    // Both names are interpolated into quoted DSL and LogQL string
    // literals; a `"` or `\` (legal in OTLP attributes) would break or
    // change either query. Rather than implement escaping for two query
    // languages, a pair whose names fall outside this conservative set is
    // simply not a candidate.
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ' '))
    };
    let mut candidates = Vec::new();
    for (svc, (bands, poison)) in per_service {
        if !safe(svc) {
            continue;
        }
        for &threshold in bands.keys() {
            let selected: Vec<(&String, u64)> = bands
                .iter()
                .filter(|&(&n, _)| n >= threshold)
                .flat_map(|(_, texts)| texts.iter().map(|(t, c)| (t, *c)))
                .collect();
            let rows: u64 = selected.iter().map(|&(_, c)| c).sum();
            let mut texts: Vec<&String> = selected.iter().map(|&(t, _)| t).collect();
            texts.sort();
            texts.dedup();
            let [text] = texts.as_slice() else {
                continue;
            };
            if !safe(text) || !(1..=4000).contains(&rows) {
                continue;
            }
            let text_total: u64 = bands
                .values()
                .filter_map(|texts| texts.get(text.as_str()))
                .sum();
            if text_total != rows {
                // Rows with this text exist below the threshold — the
                // LogQL side would return more than the DSL side.
                continue;
            }
            if poison
                .iter()
                .any(|(&n, texts)| n >= threshold || texts.contains_key(text.as_str()))
            {
                continue;
            }
            candidates.push((rows, threshold, svc, *text));
        }
    }
    candidates
}

/// One `query_range` call returning the lines plus BOTH of Loki's byte
/// figures from the same response body: engine-level decompressed
/// `totalBytesProcessed`, and the storage-side compressed/head-chunk
/// figures — so the report can carry the conservative ratio.
async fn loki_query_with_stats(
    http: &reqwest::Client,
    base: &str,
    logql: &str,
    start: u64,
    end: u64,
) -> (Vec<LineKey>, u64, LokiFetchedBytes) {
    let resp = http
        .get(format!("{base}/loki/api/v1/query_range"))
        .query(&[
            ("query", logql),
            ("start", &start.to_string()),
            ("end", &end.to_string()),
            ("limit", "5000"),
            ("direction", "forward"),
        ])
        .send()
        .await
        .expect("query_range");
    let status = resp.status();
    let body = resp.text().await.expect("query_range body");
    assert!(status.is_success(), "loki query_range {status}: {body}");
    (
        parse_loki_streams(&body).expect("parse loki streams"),
        parse_loki_bytes_processed(&body).expect("parse loki bytes"),
        parse_loki_fetched_bytes(&body).expect("parse loki fetched bytes"),
    )
}

/// One Loki **metric** `query_range` call — the L4 matrix counterpart of
/// [`loki_query_with_stats`]: `step` is pinned to the bucket width so
/// every evaluation instant is `t = bucket_start + width`
/// ([`parse_loki_matrix`]'s bucket-alignment convention), and the same
/// bytes-processed / fetched-bytes figures are read from the identical
/// stats block the line-returning pairs use.
async fn loki_query_matrix(
    http: &reqwest::Client,
    base: &str,
    logql: &str,
    start_ns: u64,
    end_ns: u64,
    bucket_width_ns: u64,
    label_name: &str,
) -> L4Measured {
    // `bucket_width_seconds` — the only producer of a bucket width this
    // harness ever queries with — guarantees a whole-second width, so
    // this truncation is exact, not a precision loss.
    let step_s = (bucket_width_ns / 1_000_000_000).to_string();
    let resp = http
        .get(format!("{base}/loki/api/v1/query_range"))
        .query(&[
            ("query", logql),
            ("start", &start_ns.to_string()),
            ("end", &end_ns.to_string()),
            ("step", &step_s),
        ])
        .send()
        .await
        .expect("query_range (matrix)");
    let status = resp.status();
    let body = resp.text().await.expect("query_range (matrix) body");
    assert!(
        status.is_success(),
        "loki query_range (matrix) returned {status}: {body}",
    );
    (
        parse_loki_matrix(&body, label_name, bucket_width_ns).expect("parse loki matrix"),
        parse_loki_bytes_processed(&body).expect("parse loki bytes"),
        parse_loki_fetched_bytes(&body).expect("parse loki fetched bytes"),
    )
}

/// Push a whole OTLP/JSON Lines corpus into Loki, batched by **encoded
/// bytes** ([`FLUSH_BYTES`] per request — sized for Loki's stock 4 MiB
/// internal gRPC cap *after* its OTLP→logproto inflation, see the
/// constant's doc) with a 500-`LogsData` secondary cap, via the retrying
/// pusher. Byte-capped because count-capped batching is blind to
/// heterogeneous batch sizes — run #2 died on a 500-batch push that
/// encoded to 5.28 MB (503 `ResourceExhausted`); adapting the pusher to
/// Loki's stock limit is the anti-strawman direction (a real OTLP
/// exporter batches under size limits too).
async fn push_corpus_to_loki(http: &reqwest::Client, base: &str, corpus_dir: &std::path::Path) {
    use prost::Message as _;
    use std::io::BufRead as _;

    /// The 4 MiB that matters is Loki's INTERNAL gRPC message, not our
    /// HTTP body: run #3 — under this constant's PREVIOUS value of
    /// 3 MiB — proved Loki's OTLP→logproto translation INFLATES the
    /// content (a ≤3 MiB push produced a 5,276,869-byte internal message,
    /// ≥1.76×, because OTLP shares resource/scope attributes per batch
    /// while the internal push repeats labels and structured metadata per
    /// entry). The current 1.5 MB gives ≥2.6× inflation headroom under
    /// the cap; `push_otlp` still asserts our own encoded size as a floor
    /// guarantee.
    const FLUSH_BYTES: usize = 1_500_000;

    let mut paths: Vec<_> = std::fs::read_dir(corpus_dir)
        .expect("read corpus dir")
        .filter_map(|e| {
            let p = e.expect("dir entry").path();
            (p.extension().and_then(|x| x.to_str()) == Some("jsonl")).then_some(p)
        })
        .collect();
    paths.sort();

    let mut pending: Vec<opentelemetry_proto::tonic::logs::v1::ResourceLogs> = Vec::new();
    let (mut pending_bytes, mut pending_lines) = (0usize, 0u64);
    let (mut batched, mut pushed) = (0u64, 0u64);
    for path in paths {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            let line_bytes: usize = data
                .resource_logs
                .iter()
                .map(|rl| rl.encoded_len() + 8)
                .sum();
            // Flush BEFORE appending if this line would overflow the byte
            // cap — so no push ever exceeds it (a single oversized line
            // still goes alone; none in practice approaches 3 MiB).
            if !pending.is_empty()
                && (pending_bytes + line_bytes > FLUSH_BYTES || pending_lines >= 500)
            {
                let payload =
                    opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
                        resource_logs: std::mem::take(&mut pending),
                    }
                    .encode_to_vec();
                push_otlp(http, base, payload).await;
                (pending_bytes, pending_lines) = (0, 0);
                pushed += 1;
                if pushed % 500 == 0 {
                    eprintln!(
                        "loki ingest: {batched} LogsData lines read, {pushed} requests sent…"
                    );
                }
            }
            pending.extend(data.resource_logs);
            pending_bytes += line_bytes;
            pending_lines += 1;
            batched += 1;
        }
    }
    if !pending.is_empty() {
        let payload = opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
            resource_logs: pending,
        }
        .encode_to_vec();
        push_otlp(http, base, payload).await;
        pushed += 1;
    }
    eprintln!("loki ingest complete: {batched} LogsData lines in {pushed} requests");
}

/// Timed repetitions per pair per system for the §3.6 latency channel.
/// Every repetition runs AFTER the pair's correctness measurement, so
/// all of them — including the first — may be cache-warm on both sides:
/// the reported figure is a **warm p50** (median, never min, so one
/// especially-warm rep cannot masquerade as the typical cost).
const LATENCY_REPS: usize = 7;

/// Median wall time of a rep set: odd lengths take the middle sample,
/// even lengths the mean of the two middles. `None` on an empty set.
fn median_duration(samples: &[Duration]) -> Option<Duration> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[mid]),
        _ => Some((sorted[mid - 1] + sorted[mid]) / 2),
    }
}

/// The RFC0031.7 latency floor over the measured p50s, through the
/// shared checked gate math ([`ourios_bench::bytes_within_floor`] —
/// its arithmetic is unit-agnostic) on **nanosecond** integers:
/// millisecond units would truncate a sub-millisecond in-process Ourios
/// p50 to 0 and trip the gate's zero-measurement guard.
fn latency_floor_gate(
    ourios: Duration,
    loki: Duration,
    factor: u64,
) -> ourios_bench::BytesGateOutcome {
    // A p50 past u64::MAX nanoseconds (~584 years) is a broken clock;
    // saturating keeps the gate's own guards in charge of failing it.
    let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    ourios_bench::bytes_within_floor(nanos(ourios), nanos(loki), factor)
}

/// The Ourios half of the §3.6 latency channel: [`LATENCY_REPS`] timed
/// end-to-end repetitions of the pair's own query through
/// [`ourios_bench::ourios_query_answer`] — the honest per-query cost
/// (count scan + row materialization + template-map acquisition, fresh
/// querier and runtime per rep; once the RFC 0033 write-through has
/// published, the acquisition is one warm artifact GET — the production
/// steady state), median wall time. Ourios is timed
/// **in-process** (no HTTP layer) while Loki is timed over localhost
/// HTTP: negligible against multi-second scans, not against
/// sub-millisecond answers — the report carries that caveat, and
/// latency stays corroborating, not sole-gating (RFC 0031 §2.5). A
/// failed repetition logs loudly and yields `None` ("latency:
/// unmeasured") rather than failing the run.
fn ourios_latency_p50(
    bucket_root: &std::path::Path,
    tenant: &TenantId,
    spec: &PairSpec,
) -> Option<Duration> {
    let mut samples = Vec::with_capacity(LATENCY_REPS);
    for _ in 0..LATENCY_REPS {
        let start = std::time::Instant::now();
        if let Err(e) =
            ourios_bench::ourios_query_answer(bucket_root, tenant, &spec.dsl, spec.now, spec.window)
        {
            eprintln!(
                "LATENCY UNMEASURED (ourios) [{}]: a timed repetition failed: {e:?}",
                spec.label,
            );
            return None;
        }
        samples.push(start.elapsed());
    }
    median_duration(&samples)
}

/// The Loki half of the §3.6 latency channel: [`LATENCY_REPS`] timed
/// `query_range` round trips of EXACTLY the measurement query (same
/// `LogQL`, window, limit, direction as [`loki_query_with_stats`]),
/// median wall time, the body drained inside the timing — delivering
/// the result is part of the cost being measured. Runs post-poll
/// ([`loki_measure_pair`] succeeded), so ingest has caught up and the
/// reps are cache-warm, mirroring the Ourios side. A failed repetition
/// logs loudly and yields `None` rather than failing the run.
///
/// Reps run before the CROSS-SYSTEM equivalence assert (which needs
/// every pair measured first), but nothing is ever REPORTED from a run
/// where equivalence broke — the run panics before the report prints —
/// so published latencies always come from equivalence-held runs.
async fn loki_latency_p50(http: &reqwest::Client, base: &str, spec: &PairSpec) -> Option<Duration> {
    // The heavy strings (URL, window bounds) are built outside the
    // timed region; the per-rep request builder still runs inside it —
    // a client constructing its request is part of an honest round
    // trip. The body is chunk-drained (not buffered) inside the timing:
    // delivery is part of the measured cost, a whole-body buffer is not.
    let url = format!("{base}/loki/api/v1/query_range");
    let (start_s, end_s) = (spec.start.to_string(), spec.end.to_string());
    let mut samples = Vec::with_capacity(LATENCY_REPS);
    for _ in 0..LATENCY_REPS {
        let start = std::time::Instant::now();
        let sent = http
            .get(&url)
            .query(&[
                ("query", spec.logql.as_str()),
                ("start", start_s.as_str()),
                ("end", end_s.as_str()),
                ("limit", "5000"),
                ("direction", "forward"),
            ])
            .send()
            .await;
        let outcome = match sent {
            Ok(mut resp) => {
                let status = resp.status();
                let drained = loop {
                    match resp.chunk().await {
                        Ok(Some(_)) => {}
                        Ok(None) => break Ok(()),
                        Err(e) => break Err(format!("body read: {e}")),
                    }
                };
                match drained {
                    Ok(()) if status.is_success() => Ok(()),
                    Ok(()) => Err(format!("HTTP {status}")),
                    Err(detail) => Err(detail),
                }
            }
            Err(e) => Err(format!("transport: {e}")),
        };
        if let Err(detail) = outcome {
            eprintln!(
                "LATENCY UNMEASURED (loki) [{}]: a timed repetition failed: {detail}",
                spec.label,
            );
            return None;
        }
        samples.push(start.elapsed());
    }
    median_duration(&samples)
}

/// Which direction a pair's bytes gate asks its question in (RFC 0031
/// §2 dispositions): the L1–L4 classes must WIN by the margin, the
/// L6/L7 family must merely stay WITHIN the floor factor.
#[derive(Clone, Copy)]
enum GateKind {
    MustWin,
    Floor,
}

impl GateKind {
    fn evaluate(self, ourios: u64, loki: u64, calibration: u64) -> ourios_bench::BytesGateOutcome {
        match self {
            Self::MustWin => ourios_bench::bytes_must_win(ourios, loki, calibration),
            Self::Floor => ourios_bench::bytes_within_floor(ourios, loki, calibration),
        }
    }
}

/// The RFC 0031 taxonomy class a pair belongs to: which §7 value
/// applies, on which channel, and whether it is FROZEN (asserted by the
/// dispatch run) or deferred/diagnostic (reported only) — per the §7
/// partial freeze (2026-07-13, calibrated against `benchmarks.md`
/// §9.13) and the `M_L2` unfreeze (2026-07-14, §9.15).
#[derive(Clone, Copy)]
enum PairClass {
    /// L1 template lookup — must-win, `M_L1 = 10` frozen on the
    /// storage channel.
    L1,
    /// L2 attribute predicate — must-win, `M_L2 = 10` frozen on the
    /// processed channel (primary) plus a frozen 1.1× storage-side
    /// floor (the 2026-07-14 unfreeze: RFC 0033's artifact moved the
    /// storage channel past parity).
    L2,
    /// L3 trace correlation — must-win, `M_L3 = 10` frozen on the
    /// storage channel.
    L3,
    /// L4 frequency aggregation — must-win (RFC 0031 §3.4), but `M_L4`
    /// stays §7-DEFERRED until first measured: both bytes channels are
    /// PRINTED, never asserted (`frozen_gate_failures` skips this class).
    L4,
    /// L6-family window browse — `F_L6 = 3` frozen on the LATENCY
    /// channel (RFC0031.7 as written); the bytes channel is a
    /// published diagnostic, not a gate.
    WindowFloor,
    /// Selective-resource window — diagnostic only; nothing gates.
    Diagnostic,
}

impl PairClass {
    /// The bytes-gate direction the class's channels report under.
    fn gate(self) -> GateKind {
        match self {
            Self::L1 | Self::L2 | Self::L3 | Self::L4 => GateKind::MustWin,
            Self::WindowFloor | Self::Diagnostic => GateKind::Floor,
        }
    }
}

/// One measured query of the indicative run: the equivalent DSL/`LogQL`
/// question, the window both systems answer it over, and the row count
/// both must return exactly.
#[derive(Clone)]
struct PairSpec {
    label: String,
    /// The pair's §7 calibration value (`m_l1`/`m_l2`/`m_l3` for the
    /// must-win classes; `f_l6` for the window slices — the latency
    /// floor factor, since the §7 freeze reclassified their bytes
    /// channel to a diagnostic).
    margin: u64,
    /// The taxonomy class: which channel gates, which is reported, and
    /// whether the §7 value is frozen or deferred.
    class: PairClass,
    dsl: String,
    logql: String,
    /// Loki `query_range` window (nanoseconds, `[start, end)` by the
    /// clean-edge construction).
    start: u64,
    end: u64,
    expected_rows: u64,
    /// The Ourios querier's window parameters (`time_window_filter` is
    /// `ts ≥ now − window ∧ ts < now`). For the time-window slices these
    /// map exactly to `[start, end)` (`now = end`, `window = end −
    /// start`); the severity pair instead uses the full-corpus
    /// effective-time window — there both sides' windows are supersets
    /// of every matching row and the severity predicate does the
    /// selecting.
    now: u64,
    window: u64,
}

/// The run's pairs — a selectivity curve plus the L-class points, not a
/// single number. Run #6 measured ONE extreme-selectivity pair (1 row)
/// and found storage-side advantage 5.95×: Ourios's fixed per-query
/// reads dominate when the answer is a single row. The curve tests the
/// amortization prediction — Ourios's fixed cost should wash out as the
/// result grows while Loki's scan grows with it. The severity pair is
/// the L2-family point; the two time-window slices (~100 and ~2000 rows
/// on the same service) are broad scans, so their gates report under
/// the L6 floor factor. The trace pair is the L3 must-win; the template
/// pair is the L1 must-win — the taxonomy's flagship class.
/// Everything the pickers produced, bundled for [`build_pair_specs`].
struct Picks<'a> {
    pair: &'a SelectivePair,
    clean_ts: &'a [u64],
    poison_ts: &'a [u64],
    trace: Option<&'a (String, u64)>,
    template: Option<&'a TemplatePair>,
    rare_window: Option<&'a (String, u64, u64, u64)>,
}

fn build_pair_specs(picks: &Picks<'_>, corpus_now: u64, corpus_window: u64) -> Vec<PairSpec> {
    let (pair, clean_ts, poison_ts) = (picks.pair, picks.clean_ts, picks.poison_ts);
    let margins = ourios_bench::ComparativeMargins::default();
    let mut specs = vec![PairSpec {
        label: format!(
            "severity, L2 family: service={} severity>={} (text={:?})",
            pair.service, pair.threshold, pair.text
        ),
        margin: margins.m_l2,
        class: PairClass::L2,
        dsl: format!(
            "service == \"{}\" and severity >= {} | limit 5000",
            pair.service, pair.threshold
        ),
        logql: format!(
            "{{service_name=\"{}\"}} | severity_text=\"{}\"",
            pair.service, pair.text
        ),
        start: pair.min_ts,
        end: pair
            .max_ts
            .checked_add(1)
            .expect("corpus max timestamp overflows the Loki window end"),
        expected_rows: pair.rows,
        now: corpus_now,
        window: corpus_window,
    }];
    for k in [100usize, 2000] {
        let Some((start, end)) = pick_window_pair(clean_ts, poison_ts, k) else {
            panic!(
                "no clean {k}-row window on service {} ({} clean rows, {} poison)",
                pair.service,
                clean_ts.len(),
                poison_ts.len(),
            );
        };
        specs.push(PairSpec {
            label: format!(
                "time-window slice, L6 family: service={} k={k}",
                pair.service
            ),
            margin: margins.f_l6,
            class: PairClass::WindowFloor,
            dsl: format!("service == \"{}\" | limit 5000", pair.service),
            logql: format!("{{service_name=\"{}\"}}", pair.service),
            start,
            end,
            expected_rows: k as u64,
            now: end,
            window: end - start,
        });
    }
    specs.extend(class_pair_specs(picks, &margins, corpus_now, corpus_window));
    specs
}

/// The L-class (L3/L1) must-win pairs plus the selective-resource
/// diagnostic — split from [`build_pair_specs`] so each half stays
/// readable.
fn class_pair_specs(
    picks: &Picks<'_>,
    margins: &ourios_bench::ComparativeMargins,
    corpus_now: u64,
    corpus_window: u64,
) -> Vec<PairSpec> {
    let (pair, trace, template, rare_window) =
        (picks.pair, picks.trace, picks.template, picks.rare_window);
    let mut specs = Vec::new();
    // L3 must-win: an exact trace lookup over the full corpus window.
    // Loki cannot pre-narrow a trace to a stream (`.+` selector +
    // structured-metadata filter = a scan across every service); Ourios
    // compiles it to a trace_id column equality. Skipping when no
    // eligible trace exists is LOUD (stderr + a 3-section report), never
    // silent.
    match trace {
        Some((hex, rows)) => specs.push(PairSpec {
            label: format!("trace correlation, L3 family: trace_id={hex}"),
            margin: margins.m_l3,
            class: PairClass::L3,
            dsl: format!("trace_id == \"{hex}\" | limit 5000"),
            logql: format!("{{service_name=~\".+\"}} | trace_id=\"{hex}\""),
            start: pair.min_ts,
            end: pair
                .max_ts
                .checked_add(1)
                .expect("corpus max timestamp overflows the Loki window end"),
            expected_rows: *rows,
            now: corpus_now,
            window: corpus_window,
        }),
        None => eprintln!(
            "L3 PAIR SKIPPED: no eligible trace (16-byte id, no zero-ts/empty-service rows, \
             2..=100 rows) in the corpus"
        ),
    }
    // L1 must-win — the taxonomy's flagship: DSL `template_id == N` rides
    // the writer's existing bloom filter on template_id; Loki has no
    // template concept, so its honest equivalent is a line filter over
    // every stream. The picker proved the two select IDENTICAL row sets
    // (needle-count == template row count + rendered-row containment).
    match template {
        Some(t) => specs.push(PairSpec {
            label: format!(
                "template lookup, L1 family: template_id={} needle={:?}",
                t.template_id, t.needle
            ),
            margin: margins.m_l1,
            class: PairClass::L1,
            dsl: format!("template_id == {} | limit 5000", t.template_id),
            logql: format!("{{service_name=~\".+\"}} |= \"{}\"", t.needle),
            start: pair.min_ts,
            end: pair
                .max_ts
                .checked_add(1)
                .expect("corpus max timestamp overflows the Loki window end"),
            expected_rows: t.rows,
            now: corpus_now,
            window: corpus_window,
        }),
        None => eprintln!(
            "L1 PAIR SKIPPED: no template with a validated constant needle (≥ 10 safe \
             chars, 2..=4000 rows, needle-count == template row count, no \
             zero-ts/empty-service match) in the corpus"
        ),
    }
    // Selective-resource DIAGNOSTIC (not an L-class gate): the same
    // window-browse shape as the L6 pairs but scoped to the corpus's
    // LOWEST-volume service, where the promoted service.name bloom can
    // actually skip row groups — the L6 pairs use the highest-volume
    // service, the bloom's worst case, so they bound the unscoped-browse
    // cost while this bounds the enriched one.
    match rare_window {
        Some((service, start, end, rows)) => specs.push(PairSpec {
            label: format!("selective-resource window, diagnostic: service={service} k={rows}"),
            margin: margins.f_l6,
            class: PairClass::Diagnostic,
            dsl: format!("service == \"{service}\" | limit 5000"),
            logql: format!("{{service_name=\"{service}\"}}"),
            start: *start,
            end: *end,
            expected_rows: *rows,
            now: *end,
            window: *end - *start,
        }),
        None => eprintln!(
            "SELECTIVE-RESOURCE PAIR SKIPPED: no low-volume service with a clean \
             2..=100-row window in the corpus"
        ),
    }
    specs
}

/// One pair's Loki measurement: poll `query_range` until ingest has
/// caught up to the expected row count. On a deadline miss it emits a
/// diagnostic dump (itself bounded to 90 s) and then returns `Err`
/// instead of panicking, so one pair's failure cannot destroy the other
/// pairs' already-measured report (run #11 lost three pairs'
/// measurements to one L3 timeout panic).
async fn loki_measure_pair(
    http: &reqwest::Client,
    base: &str,
    spec: &PairSpec,
) -> Result<(Vec<LineKey>, u64, LokiFetchedBytes), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let (lines, bytes, fetched) =
            loki_query_with_stats(http, base, &spec.logql, spec.start, spec.end).await;
        if lines.len() as u64 >= spec.expected_rows {
            break Ok((lines, bytes, fetched));
        }
        if std::time::Instant::now() >= deadline {
            // Bounded so diagnostics can't extend the miss indefinitely.
            if tokio::time::timeout(
                Duration::from_secs(90),
                dump_loki_diagnostics(http, base, spec),
            )
            .await
            .is_err()
            {
                eprintln!(
                    "(diagnostics dump for [{}] itself timed out after 90 s)",
                    spec.label,
                );
            }
            break Err(format!(
                "loki returned {} of {} expected rows for [{}] before timeout",
                lines.len(),
                spec.expected_rows,
                spec.label,
            ));
        }
        // 10 s, not 2 s: an expensive pair's poll (the L3 full-corpus
        // scan runs ~19 s of engine time) must not queue up behind
        // itself — run #13's failing query showed 321 s of queueTime
        // from 2 s polling, which ate the deadline in Loki's queue.
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// The L4 pair's Loki measurement: poll the matrix `query_range` until
/// ingest has caught up to the expected total row count — the
/// aggregation counterpart of [`loki_measure_pair`]. Returns `Err`
/// instead of panicking on a deadline miss, so an L4 failure cannot
/// destroy the already-measured/printed evidence for the other pairs
/// (same run #11 salvage lesson; L4 is measured and reported last, see
/// `rfc0031_indicative_comparative_run`).
///
/// Deadline is longer than [`loki_measure_pair`]'s 300 s. Before
/// `-validation.max-entries-limit` was raised, widening this deadline
/// alone (run #7, 900 s) made no measurable difference — proof the
/// shortfall was a hard per-query cap, not a timing race. With that cap
/// raised (run #8), completeness jumped from a ~93% plateau to 97.1%,
/// which now behaves like genuine ingest settle time rather than a
/// fixed ceiling — worth confirming with real headroom before
/// concluding a third factor is still capping it.
async fn loki_measure_frequency_pair(
    http: &reqwest::Client,
    base: &str,
    spec: &PairSpec,
    bucket_width_ns: u64,
) -> Result<L4Measured, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    loop {
        let (groups, bytes, fetched) = loki_query_matrix(
            http,
            base,
            &spec.logql,
            spec.start,
            spec.end,
            bucket_width_ns,
            "value",
        )
        .await;
        let total: u64 = groups.values().sum();
        if total >= spec.expected_rows {
            break Ok((groups, bytes, fetched));
        }
        if std::time::Instant::now() >= deadline {
            break Err(format!(
                "loki returned {total} of {} expected rows for [{}] before timeout",
                spec.expected_rows, spec.label,
            ));
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Split successes from failures: equivalence + report run for every
/// measured pair BEFORE the run fails on the broken ones, so one pair's
/// timeout cannot destroy the others' 40-minute measurements. The
/// trailing `Option<Duration>` is the pair's Loki `latency_p50` —
/// `None` when the (corroborating) latency channel failed on an
/// otherwise-good pair, reported as "unmeasured".
type Measured = (Vec<LineKey>, u64, LokiFetchedBytes, Option<Duration>);

/// The L4 pair's Loki measurement — the aggregation counterpart of
/// [`Measured`]: a `(bucket, group) -> count` map rather than a
/// [`LineKey`] multiset, with no latency channel this slice wires (see
/// [`run_l4_pair`]'s doc).
type L4Measured = (HashMap<AggKey, u64>, u64, LokiFetchedBytes);

/// One pair's Ourios measurement: the correctness answer plus the §3.6
/// latency channel (`None` = unmeasured; latency never fails the run).
#[derive(Clone)]
struct OuriosMeasured {
    answer: ourios_bench::OuriosAnswer,
    latency_p50: Option<Duration>,
    /// The RFC 0033 template-map acquisition + publish-outcome label
    /// behind `answer.registry_bytes` — cold audit fold vs warm
    /// artifact GET, with the publish outcome the §3.2 amendment
    /// requires printed explicitly ([`TemplateMapProbe`]).
    template_map: String,
}

/// One pair's artifact observation, taken right after its query (the
/// state changes across pairs — the first cold miss publishes for the
/// rest): a warm hit's acquisition equals the published artifact's byte
/// size exactly (the only registry-path GET is the artifact); anything
/// else is the cold audit fold. The absent arm's publish outcome —
/// `abstained` vs `error`, run #20's ambiguity — is resolved once,
/// after every pair's timed measurement
/// ([`reproduce_publish_decision`]). `lost_race` cannot occur here: the
/// harness is the store's only writer, so it is never printed.
enum TemplateMapProbe {
    Warm {
        artifact_bytes: u64,
    },
    ColdPublished {
        registry_bytes: u64,
        artifact_bytes: u64,
    },
    ColdAbsent {
        registry_bytes: u64,
    },
    ColdUnreadable {
        registry_bytes: u64,
        detail: String,
    },
}

impl TemplateMapProbe {
    fn observe(artifact: &std::path::Path, registry_bytes: u64) -> Self {
        match std::fs::metadata(artifact) {
            Ok(meta) if meta.len() == registry_bytes => Self::Warm {
                artifact_bytes: registry_bytes,
            },
            Ok(meta) => Self::ColdPublished {
                registry_bytes,
                artifact_bytes: meta.len(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Self::ColdAbsent { registry_bytes }
            }
            Err(e) => Self::ColdUnreadable {
                registry_bytes,
                detail: e.to_string(),
            },
        }
    }

    /// The report label; `absent_outcome` is the once-computed
    /// [`reproduce_publish_decision`] string (present iff any pair
    /// observed absence).
    fn label(&self, absent_outcome: Option<&str>) -> String {
        match self {
            Self::Warm { artifact_bytes } => {
                format!("warm (one artifact GET, {artifact_bytes} B compressed)")
            }
            Self::ColdPublished {
                registry_bytes,
                artifact_bytes,
            } => format!(
                "cold (audit fold, {registry_bytes} B; published — artifact \
                 {artifact_bytes} B compressed)"
            ),
            Self::ColdAbsent { registry_bytes } => format!(
                "cold (audit fold, {registry_bytes} B; {})",
                absent_outcome.expect("an absent probe resolves its publish outcome"),
            ),
            Self::ColdUnreadable {
                registry_bytes,
                detail,
            } => format!("cold (audit fold, {registry_bytes} B; artifact stat failed: {detail})"),
        }
    }
}

/// Resolve run #20's abstained-vs-error ambiguity for a store left with
/// no artifact (RFC 0033 §3.2 amendment: the harness MUST print each
/// pair's publish outcome): reproduce the §3.5 publish decision — one
/// fold + serialize + compress, the exact bytes the write-through would
/// have published — against the folded audit bytes. Off the measured
/// path by construction: called once, after every pair's timed
/// measurement, and only labels.
fn reproduce_publish_decision(bucket: &std::path::Path, tenant: &TenantId) -> String {
    let (map, fold_bytes) = match ourios_querier::derive_template_map(
        ourios_querier::StoreRef::Local(bucket),
        tenant,
    ) {
        Ok(derived) => derived,
        Err(e) => return format!("publish outcome unresolvable — refold failed: {e}"),
    };
    match map.to_artifact_bytes() {
        Ok(bytes) if (bytes.len() as u64) < fold_bytes => format!(
            "publish error — would-be artifact {} B compressed < folded audit \
             {fold_bytes} B, yet nothing on the store",
            bytes.len(),
        ),
        Ok(bytes) => format!(
            "abstained — would-be artifact {} B compressed >= folded audit {fold_bytes} B",
            bytes.len(),
        ),
        Err(e) => format!("publish error — serialization failed: {e}"),
    }
}

/// The RFC0033.6 corpus acquisition gate (§5.6 as amended 2026-07-14,
/// the run #21 record): when any measured pair ran warm, the warm
/// artifact GET must cost at most half the cold audit fold —
/// `warm × 2 ≤ fold`, integer-exact. The fold figure comes from a cold
/// pair's measurement when the run has one; the healthy steady state is
/// all-warm (run #21), so the caller refolds once via
/// `derive_template_map` — off the timed path, after every pair's reps,
/// exactly like the publish-outcome resolution. No warm pair means the
/// gate has nothing to decide (run #20's all-cold shape is RFC 0033's
/// own red, not this gate's); a failed refold makes it non-evaluable,
/// reported LOUDLY rather than failed, mirroring the RFC0031.7
/// unmeasured-latency stance. Returns the failure in the
/// `frozen_gate_failures` message shape so the caller asserts them
/// together, AFTER the report has printed.
fn template_map_acquisition_failure(
    warm_artifact: Option<u64>,
    cold_fold: Option<u64>,
    refold: impl FnOnce() -> Result<u64, String>,
) -> Option<String> {
    let warm = warm_artifact?;
    let fold = match cold_fold.map_or_else(refold, Ok) {
        Ok(fold) => fold,
        Err(e) => {
            eprintln!("RFC0033.6 not evaluable this run (refold failed: {e})");
            return None;
        }
    };
    if warm == 0 || fold == 0 {
        // The lgates honesty rule: a zero measurement must not decide a
        // gate — least of all fake a pass.
        return Some(format!(
            "[template map] RFC0033.6 corpus acquisition gate invalid: zero byte-count \
             (warm={warm}, fold={fold})"
        ));
    }
    let pass = warm.checked_mul(2).is_some_and(|double| double <= fold);
    (!pass).then(|| {
        format!(
            "[template map] RFC0033.6 corpus acquisition gate (warm ≤ fold/2) failed: \
             warm artifact GET {warm} B vs cold audit fold {fold} B"
        )
    })
}

#[allow(clippy::type_complexity)] // one-shot plumbing tuple for the runner
fn split_measurements(
    specs: &[PairSpec],
    ourios: &[OuriosMeasured],
    loki: Vec<Result<Measured, String>>,
) -> (
    Vec<PairSpec>,
    Vec<OuriosMeasured>,
    Vec<Measured>,
    Vec<String>,
) {
    assert!(
        specs.len() == ourios.len() && specs.len() == loki.len(),
        "harness bug: {} specs vs {} ourios vs {} loki measurements — zip \
         would silently drop the tail",
        specs.len(),
        ourios.len(),
        loki.len(),
    );
    let (mut ok_specs, mut ok_ourios, mut ok_loki, mut failures) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ((spec, ours), result) in specs.iter().zip(ourios).zip(loki) {
        match result {
            Ok(measured) => {
                ok_specs.push(spec.clone());
                ok_ourios.push(ours.clone());
                ok_loki.push(measured);
            }
            Err(detail) => failures.push(detail),
        }
    }
    (ok_specs, ok_ourios, ok_loki, failures)
}

/// The §7-frozen gate checks (partial freeze 2026-07-13; `M_L2` unfrozen
/// and frozen 2026-07-14): L1/L3 must-win on the storage channel at
/// their frozen margins; L2 must-win on the processed channel (primary)
/// AND hold the frozen 1.1× storage-side floor; the L6-family window
/// pairs hold the RFC0031.7 latency floor **when latency was measured**
/// — latency is `Option` by design, and an unmeasured p50 makes the
/// frozen gate non-evaluable, which is reported LOUDLY rather than
/// failed (a corroborating-channel hiccup must not fake a floor
/// breach). The window pairs' bytes channel and the selective-resource
/// pair are published diagnostics — never checked here. L4 is a
/// must-win class (§3.4) but `M_L4` stays §7-DEFERRED, so it is excluded
/// here too — its bytes are `print_pair_bytes_gates`' job, reported not
/// asserted, until the maintainer freezes a margin against a first
/// measurement. Returns the failures instead of panicking so the caller
/// can assert them all at once, AFTER the report has printed.
fn frozen_gate_failures(
    specs: &[PairSpec],
    ourios: &[OuriosMeasured],
    loki: &[Measured],
) -> Vec<String> {
    let mut failures = Vec::new();
    for ((spec, ours), (_, loki_processed, loki_fetched, loki_latency)) in
        specs.iter().zip(ourios).zip(loki)
    {
        let loki_storage = loki_fetched.compressed_bytes + loki_fetched.head_chunk_bytes;
        match spec.class {
            PairClass::L1 | PairClass::L3 => {
                let outcome =
                    ourios_bench::bytes_must_win(ours.answer.bytes_read, loki_storage, spec.margin);
                if !outcome.passed() {
                    failures.push(format!(
                        "[{}] frozen storage-channel must-win (margin {}) failed: {outcome:?}",
                        spec.label, spec.margin,
                    ));
                }
            }
            PairClass::L2 => {
                let outcome = ourios_bench::bytes_must_win(
                    ours.answer.bytes_read,
                    *loki_processed,
                    spec.margin,
                );
                if !outcome.passed() {
                    failures.push(format!(
                        "[{}] frozen processed-channel must-win (margin {}) failed: {outcome:?}",
                        spec.label, spec.margin,
                    ));
                }
                let tenths = ourios_bench::ComparativeMargins::default().m_l2_storage_floor_tenths;
                let outcome = ourios_bench::bytes_must_win_tenths(
                    ours.answer.bytes_read,
                    loki_storage,
                    tenths,
                );
                if !outcome.passed() {
                    failures.push(format!(
                        "[{}] frozen storage-channel floor (must-win at {tenths}/10) \
                         failed: {outcome:?}",
                        spec.label,
                    ));
                }
            }
            PairClass::WindowFloor => match (ours.latency_p50, *loki_latency) {
                (Some(ours_p50), Some(loki_p50)) => {
                    let outcome = latency_floor_gate(ours_p50, loki_p50, spec.margin);
                    if !outcome.passed() {
                        failures.push(format!(
                            "[{}] RFC0031.7 latency floor (frozen factor {}) failed: {outcome:?}",
                            spec.label, spec.margin,
                        ));
                    }
                }
                _ => eprintln!(
                    "RFC0031.7 not evaluable this run for [{}] (latency unmeasured)",
                    spec.label,
                ),
            },
            // M_L4 is §7-deferred — reported by `print_pair_bytes_gates`,
            // never asserted here.
            PairClass::L4 | PairClass::Diagnostic => {}
        }
    }
    failures
}

/// Timed-out pair post-mortem, printed to stderr so the run log carries
/// the evidence: the raw (truncated) response body of the failing query,
/// and a filterless sample of the same window so the entries' shape —
/// including whether structured metadata is present at all on replayed
/// data — is visible. Bodies are truncated to ~4 KiB (4096 chars).
async fn dump_loki_diagnostics(http: &reqwest::Client, base: &str, spec: &PairSpec) {
    let raw = |query: &str, limit: u32| {
        let query = query.to_string();
        async move {
            match http
                .get(format!("{base}/loki/api/v1/query_range"))
                .query(&[
                    ("query", query.as_str()),
                    ("start", &spec.start.to_string()),
                    ("end", &spec.end.to_string()),
                    ("limit", &limit.to_string()),
                    // Mirror the measurement request so the dump reflects
                    // exactly what the poll saw.
                    ("direction", "forward"),
                ])
                .send()
                .await
            {
                Ok(mut resp) => {
                    let status = resp.status();
                    // Stream at most ~8 KiB rather than buffering a whole
                    // (possibly huge) body just to print a snippet.
                    let mut buf: Vec<u8> = Vec::with_capacity(8192);
                    loop {
                        match resp.chunk().await {
                            Ok(Some(chunk)) => {
                                buf.extend_from_slice(&chunk);
                                if buf.len() >= 8192 {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            // A truncated-by-error body must say so, or the
                            // post-mortem reads as a complete response.
                            Err(e) => {
                                buf.extend_from_slice(format!("<body read error: {e}>").as_bytes());
                                break;
                            }
                        }
                    }
                    // Truncate on a char boundary; the cap is ~4 KiB of
                    // ASCII JSON, not an exact byte count.
                    let body: String = String::from_utf8_lossy(&buf).chars().take(4096).collect();
                    format!("HTTP {status}: {body}")
                }
                Err(e) => format!("transport error: {e}"),
            }
        }
    };
    eprintln!(
        "=== diagnostics for timed-out pair [{}] ===\nfailing query {:?} =>\n{}\n\n\
         filterless sample of the same window =>\n{}\n=== end diagnostics ===",
        spec.label,
        spec.logql,
        raw(&spec.logql, 5000).await,
        raw("{service_name=~\".+\"}", 3).await,
    );
}

/// The §7 calibration input: the indicative Ourios-vs-Loki bytes-read
/// comparison on a real corpus (`OURIOS_COMPARATIVE_CORPUS`, fetched by
/// the `comparative-bench` dispatch workflow — the `corpus/otel-demo-v*`
/// releases), measured across the [`build_pair_specs`] pair set — a
/// selectivity curve plus the L1/L3 must-win points — over one
/// container + one corpus replay.
///
/// **Equivalence is asserted per pair, and the §7-FROZEN gates
/// (partial freeze 2026-07-13; `M_L2` unfrozen and frozen 2026-07-14)
/// are asserted after the report prints**:
/// L1/L3 storage-channel must-win at margin 10, L2 processed-channel
/// must-win at 10 plus the 1.1× storage-side floor (11/10,
/// integer-exact), the RFC0031.7 latency floor at
/// factor 3 on the L6-family window pairs (loudly non-evaluable when
/// latency is unmeasured, never a silent pass or a spurious fail), and
/// the RFC0033.6 corpus acquisition gate (warm artifact GET ≤ half the
/// cold audit fold, whenever any pair ran warm). The
/// window pairs' bytes channel and the
/// selective-resource pair stay reported diagnostics per the §7
/// deferrals.
///
/// Loki runs the stock image config plus explicit, documented
/// ingest-side deviations (all in LOKI'S favour — the anti-strawman
/// direction): `-validation.reject-old-samples=false` (the frozen
/// captures carry their original timestamps, weeks old by run time),
/// raised ingest-rate limits so a 2.96 GB replay isn't throttled by
/// dev-scale defaults, and a raised internal gRPC message cap (see the
/// flag comment). The query side stays stock.
#[test]
#[ignore = "dispatch-only: needs Docker + a corpus via OURIOS_COMPARATIVE_CORPUS (comparative-bench workflow)"]
#[allow(clippy::too_many_lines)] // one linear dispatch-run script: pick → build → measure → gate
fn rfc0031_indicative_comparative_run() {
    let corpus_dir = std::path::PathBuf::from(
        std::env::var("OURIOS_COMPARATIVE_CORPUS")
            .expect("set OURIOS_COMPARATIVE_CORPUS to a corpus dir (the dispatch workflow does)"),
    );

    let pair = pick_selective_pair(&corpus_dir);
    eprintln!("pair: {pair:?}");
    let (clean_ts, poison_ts) = collect_service_timestamps(&corpus_dir, &pair.service);
    eprintln!(
        "service {}: {} clean timestamps, {} zero-time (poison)",
        pair.service,
        clean_ts.len(),
        poison_ts.len(),
    );
    let trace = pick_trace_pair(&corpus_dir);
    eprintln!("trace pair: {trace:?}");
    let rare_window = pick_rare_window_pair(&corpus_dir, &pair.service);
    eprintln!("selective-resource window: {rare_window:?}");

    // The (locally-proven) Ourios half, per pair. The RFC 0033
    // template-map artifact persists in the bucket across pairs BY
    // DESIGN — production reality: caches persist. The first
    // row-rendering query after the store build folds the audit stream
    // cold and write-through-publishes; every later query's registry
    // component is one small artifact GET (the L1 picker's rendering
    // validation query may itself be the publisher, so even the first
    // measured pair can be warm). Each pair's outcome is classified at
    // measurement time and printed in the report block.
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        &corpus_dir,
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let corpus_now = built
        .max_effective_time_unix_nano
        .checked_add(1)
        .expect("corpus max effective timestamp overflows now");
    let corpus_window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
    // The L1 picker runs against the BUILT store (template ids exist only
    // post-mining), unlike the corpus-scanning pickers above.
    let template = pick_template_pair(
        &corpus_dir,
        bucket.path(),
        &tenant,
        corpus_now,
        corpus_window,
    );
    eprintln!("template pair: {template:?}");
    // L4 (RFC 0031 §3.4/§3.5): picked the same way as L1's template pair
    // (post-store-build — group cardinality only exists once templates
    // are mined), but kept OUT of `Picks`/`specs` — purely additive to
    // the pair list, not woven into the line-returning classes' shared
    // machinery (see `run_l4_pair`'s doc for why: an aggregation's
    // `(bucket, group) -> count` map is not a `LineKey` multiset, and
    // forcing it through `OuriosAnswer`/`compare_lines` would misrepresent
    // the state rather than model it).
    let frequency = pick_frequency_pair(bucket.path(), &tenant, corpus_now, corpus_window);
    eprintln!("frequency pair: {frequency:?}");
    let margins = ourios_bench::ComparativeMargins::default();
    let l4_spec = frequency.as_ref().and_then(|pair| {
        l4_pair_spec(
            pair,
            built.min_effective_time_unix_nano,
            built.max_effective_time_unix_nano,
            corpus_now,
            corpus_window,
            &margins,
        )
    });
    if l4_spec.is_none() {
        eprintln!(
            "L4 PAIR SKIPPED: no template/param slot in the corpus cleared \
             pick_frequency_pair's bounds (moderate cardinality {L4_CARDINALITY:?}, \
             >= {L4_MIN_ROWS} rows, a validated needle + capture regex, no backtick \
             in the capture regex)"
        );
    }
    let picks = Picks {
        pair: &pair,
        clean_ts: &clean_ts,
        poison_ts: &poison_ts,
        trace: trace.as_ref(),
        template: template.as_ref(),
        rare_window: rare_window.as_ref(),
    };
    let specs = build_pair_specs(&picks, corpus_now, corpus_window);
    let artifact_path = bucket
        .path()
        .join("audit")
        .join(format!(
            "tenant_id={}",
            ourios_parquet::percent_encode_tenant(built.tenant),
        ))
        .join(ourios_querier::TEMPLATE_MAP_FILENAME);
    let measured: Vec<(
        ourios_bench::OuriosAnswer,
        Option<Duration>,
        TemplateMapProbe,
    )> = specs
        .iter()
        .map(|spec| {
            let answer = ourios_bench::ourios_query_answer(
                bucket.path(),
                &tenant,
                &spec.dsl,
                spec.now,
                spec.window,
            )
            .expect("ourios answer");
            assert_eq!(
                answer.lines.len() as u64,
                spec.expected_rows,
                "Ourios must return exactly [{}]'s expected rows",
                spec.label,
            );
            let probe = TemplateMapProbe::observe(&artifact_path, answer.registry_bytes);
            // Timed reps only after the pair's Ourios correctness holds.
            let latency_p50 = ourios_latency_p50(bucket.path(), &tenant, spec);
            (answer, latency_p50, probe)
        })
        .collect();
    // Publish-outcome labels resolve AFTER every timed measurement (§3.2
    // amendment): reproducing the abstention decision costs a fold +
    // compress, so it runs once, off the measured path, and only labels.
    let absent_outcome = measured
        .iter()
        .any(|(_, _, probe)| matches!(probe, TemplateMapProbe::ColdAbsent { .. }))
        .then(|| reproduce_publish_decision(bucket.path(), &tenant));
    // The RFC0033.6 corpus acquisition gate's inputs, harvested before
    // the probes collapse into report labels: any warm pair's artifact
    // GET size, and — when a pair ran cold — the fold it measured.
    let warm_artifact = measured.iter().find_map(|(_, _, probe)| match probe {
        TemplateMapProbe::Warm { artifact_bytes } => Some(*artifact_bytes),
        _ => None,
    });
    let cold_fold = measured.iter().find_map(|(_, _, probe)| match probe {
        TemplateMapProbe::ColdPublished { registry_bytes, .. }
        | TemplateMapProbe::ColdAbsent { registry_bytes }
        | TemplateMapProbe::ColdUnreadable { registry_bytes, .. } => Some(*registry_bytes),
        TemplateMapProbe::Warm { .. } => None,
    });
    let acquisition_failure = template_map_acquisition_failure(warm_artifact, cold_fold, || {
        ourios_querier::derive_template_map(ourios_querier::StoreRef::Local(bucket.path()), &tenant)
            .map(|(_, fold_bytes)| fold_bytes)
            .map_err(|e| e.to_string())
    });
    let ourios: Vec<OuriosMeasured> = specs
        .iter()
        .zip(measured)
        .map(|(spec, (answer, latency_p50, probe))| {
            let template_map = probe.label(absent_outcome.as_deref());
            eprintln!(
                "[{}] template-map acquisition (RFC 0033): {template_map}",
                spec.label,
            );
            OuriosMeasured {
                answer,
                latency_p50,
                template_map,
            }
        })
        .collect();

    // The Loki half: one container (stock + documented ingest-side
    // flags), ONE full-corpus OTLP replay, all pairs measured against it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (loki, l4_loki): (Vec<_>, Option<Result<L4Measured, String>>) = runtime.block_on(async {
        let (_container, base, http) = start_loki(&[
            "-validation.reject-old-samples=false",
            // Run #11/#13 post-mortems (the #488 diagnostics): queries over
            // the replayed corpus's WEEKS-OLD time range skip the ingesters
            // entirely (`query_ingesters_within`, default 3h — the failing
            // response showed `ingester.totalReached: 0`), so rows still
            // sitting in unflushed low-volume chunks are INVISIBLE — the
            // L3 trace pair flickered with flush timing while high-volume
            // streams (kafka) always flushed fast enough to be seen. `0`
            // disables the cutoff so ingesters are always consulted — the
            // query-side twin of `reject-old-samples=false` for frozen
            // corpora, and in Loki's favour (without it Loki's answer to
            // an old-range query is silently incomplete).
            "-querier.query-ingesters-within=0",
            "-distributor.ingestion-rate-limit-mb=512",
            "-distributor.ingestion-burst-size-mb=1024",
            "-ingester.per-stream-rate-limit=512MB",
            "-ingester.per-stream-rate-limit-burst=1GB",
            // Runs #2–#4 all failed on the SAME ~5.27 MB internal message
            // regardless of our batch size (3 MiB → 1.5 MB outer), proving
            // a single `kafka`-service LogsData line's content alone
            // inflates past the stock 4 MiB internal gRPC cap. Raising the
            // cap (standard operator tuning, in Loki's favour — it lets
            // Loki accept the data at all) preserves the identical-ingest
            // precondition the equivalence check requires; skipping the
            // line would silently unequalize the two corpora. Flag names
            // per dskit's server registry (grpc_server_max_*_msg_size).
            "-server.grpc-max-recv-msg-size-bytes=16777216",
            "-server.grpc-max-send-msg-size-bytes=16777216",
            // `loki_measure_frequency_pair` polls the SAME query+step
            // repeatedly waiting for completeness; disable query_range
            // results caching so every poll re-queries fresh data rather
            // than risking a stale cached answer (flag lives under the
            // `querier.` prefix on `queryrangebase.Config.CacheResults`,
            // verified against the pinned v3.5.3 source).
            "-querier.cache-results=false",
            // L4's near-miss completeness plateau (runs #4/#6/#7 all
            // converged around 93-96% of expected rows, independent of
            // poll deadline — ruling out a slow-ingest race) is Loki's
            // default `-validation.max-entries-limit` (5000): the
            // `count_over_time(... | regexp ...)` aggregation still has
            // to scan every raw kafka log line in-window before the
            // line filter narrows it down, and kafka's overall volume in
            // some query-frontend splits exceeds 5000 lines, silently
            // truncating the scan before it reaches every matching line.
            // Confirmed by pulling the frozen otel-demo-v8 corpus and
            // checking every line matching the L4 needle against the
            // capture regex directly: all match — the shortfall isn't a
            // regex/content mismatch, it's lines never being scanned.
            // Raised well past the corpus's noisiest single template's
            // volume (~971K rows) — in Loki's favour, cost is memory,
            // not correctness.
            "-validation.max-entries-limit=2000000",
        ])
        .await;
        push_corpus_to_loki(&http, &base, &corpus_dir).await;
        let mut measured = Vec::with_capacity(specs.len());
        for spec in &specs {
            let result = match loki_measure_pair(&http, &base, spec).await {
                Ok((lines, bytes, fetched)) => {
                    // Timed reps only after the poll proved ingest caught
                    // up — post-poll, so warm-up asymmetry is minimized.
                    let latency_p50 = loki_latency_p50(&http, &base, spec).await;
                    Ok((lines, bytes, fetched, latency_p50))
                }
                Err(detail) => Err(detail),
            };
            measured.push(result);
        }
        // L4, same container + corpus replay as the pairs above (one
        // Loki instance for the whole run) — but measured on its own
        // matrix query, not folded into the `for spec in &specs` loop.
        let l4 = match (&frequency, &l4_spec) {
            (Some(pair), Some(spec)) => {
                let bucket_width_ns = bucket_width_seconds(&pair.bucket_width)
                    .checked_mul(1_000_000_000)
                    .expect("bucket width fits u64 nanoseconds");
                Some(loki_measure_frequency_pair(&http, &base, spec, bucket_width_ns).await)
            }
            _ => None,
        };
        (measured, l4)
    });

    let (ok_specs, ok_ourios, ok_loki, mut failures) = split_measurements(&specs, &ourios, loki);

    // Equivalence gates every successful measurement (RFC0031.1).
    for ((spec, ours), (loki_lines, _, _, _)) in ok_specs.iter().zip(&ok_ourios).zip(&ok_loki) {
        let outcome = compare_lines(&ours.answer.lines, loki_lines, 8);
        assert!(
            outcome.is_equal(),
            "the two systems' answers must be multiset-identical on [{}]: {outcome:?}",
            spec.label,
        );
    }

    print_indicative_report(
        &corpus_dir,
        pair.total_records,
        &ok_specs,
        &ok_ourios,
        &ok_loki,
    );

    // L4 (RFC 0031 §3.4/§3.5) is purely additive to the pair list above,
    // measured and reported right after it — BEFORE the gate/failure
    // assertions below, not after: an L1–L3/L6 measurement failure (a
    // flake, e.g. the documented Loki low-volume-chunk race) must not
    // prevent L4 from ever being attempted or reported. A failed L4
    // measurement joins the SAME `failures` list those classes use, so
    // it still fails the run — just without silently starving L4 of a
    // chance to run at all. A missing candidate (the picker found
    // nothing eligible) is a real, loud finding, not a silently-skipped
    // step — it was already reported above, at pick time.
    match (&l4_spec, l4_loki) {
        (Some(spec), Some(result)) => {
            run_l4_pair(bucket.path(), &tenant, spec, result, &mut failures);
        }
        (None, _) => {}
        (Some(spec), None) => unreachable!(
            "l4_spec is Some iff frequency is Some, which is exactly the condition the \
             async block used to decide whether to measure the Loki half — [{}] has a spec \
             but no Loki measurement (harness bug)",
            spec.label,
        ),
    }

    // The frozen gates run AFTER the report so a failed gate cannot
    // destroy the run's evidence (the run #11 salvage lesson).
    let mut gate_failures = frozen_gate_failures(&ok_specs, &ok_ourios, &ok_loki);
    gate_failures.extend(acquisition_failure);
    assert!(
        gate_failures.is_empty(),
        "{} asserting gate(s) failed — frozen §7 plus the RFC0033.6 corpus acquisition \
         gate (the report above carries the evidence): {gate_failures:#?}",
        gate_failures.len(),
    );
    assert!(
        failures.is_empty(),
        "{} pair(s) failed to measure (report above covers the rest, L4 included): \
         {failures:?}",
        failures.len(),
    );
}

/// One pair's bytes-channel lines, labeled per the §7 partial freeze
/// (2026-07-13) and the `M_L2` unfreeze (2026-07-14). Both Loki byte
/// figures are evaluated for every pair —
/// `totalBytesProcessed` is decompressed engine-side work, which
/// overstates Loki's storage reads by the chunk compression ratio; the
/// storage-side figure (compressed chunk bytes + memory-served
/// head-chunk bytes) is the apples-to-apples counterpart of Ourios's
/// fetched-compressed-Parquet bytes — but which line carries a gate
/// verdict depends on the class: L1/L3 gate on the frozen storage
/// channel (processed is context), L2 gates on the processed channel
/// (primary) plus the frozen 1.1× storage-side floor, L4 prints both
/// channels' ratio with no verdict (`M_L4` §7-deferred), and the window
/// pairs' bytes lines are a published diagnostic — a ratio, no verdict
/// — since the freeze reclassified them; their gate is the RFC0031.7
/// latency floor.
fn print_pair_bytes_gates(
    spec: &PairSpec,
    ourios_bytes: u64,
    loki_storage: u64,
    loki_processed: u64,
) {
    let gate = |loki_bytes: u64| {
        spec.class
            .gate()
            .evaluate(ourios_bytes, loki_bytes, spec.margin)
    };
    // Diagnostic bytes lines carry the gate math's advantage ratio (and
    // its zero-measurement honesty guards) WITHOUT a pass/fail verdict:
    // the §7 freeze reclassified the window pairs' bytes from gated
    // floor to published diagnostic.
    let diagnostic = |loki_bytes: u64| match gate(loki_bytes) {
        ourios_bench::BytesGateOutcome::Decided { advantage, .. } => {
            format!("ratio loki/ourios = {advantage:.3}")
        }
        outcome => format!("{outcome:?}"),
    };
    match spec.class {
        PairClass::L1 | PairClass::L3 => {
            println!(
                "gate vs storage-side (PRIMARY — §7 FROZEN, must-win margin {}): {:?}",
                spec.margin,
                gate(loki_storage),
            );
            println!(
                "gate vs bytes-processed (context, must-win margin {}): {:?}",
                spec.margin,
                gate(loki_processed),
            );
        }
        PairClass::L2 => {
            println!(
                "gate vs bytes-processed (PRIMARY — §7 FROZEN, must-win margin {}): {:?}",
                spec.margin,
                gate(loki_processed),
            );
            let tenths = ourios_bench::ComparativeMargins::default().m_l2_storage_floor_tenths;
            println!(
                "gate vs storage-side (§7 FROZEN floor, must-win at {tenths}/10): {:?}",
                ourios_bench::bytes_must_win_tenths(ourios_bytes, loki_storage, tenths),
            );
        }
        // L4 is must-win-shaped (RFC 0031 §3.4), but `M_L4` stays
        // §7-DEFERRED until first measured (§7): both channels print the
        // gate math's ratio for the record, never a pass/fail verdict —
        // `frozen_gate_failures` never evaluates this class.
        PairClass::L4 => {
            println!(
                "gate vs storage-side (must-win margin {}, §7 DEFERRED — reported only): {}",
                spec.margin,
                diagnostic(loki_storage),
            );
            println!(
                "gate vs bytes-processed (must-win margin {}, §7 DEFERRED — reported only): {}",
                spec.margin,
                diagnostic(loki_processed),
            );
        }
        PairClass::WindowFloor | PairClass::Diagnostic => {
            println!(
                "bytes vs storage-side: diagnostic (published, not gated); {}",
                diagnostic(loki_storage),
            );
            println!(
                "bytes vs bytes-processed: diagnostic (published, not gated); {}",
                diagnostic(loki_processed),
            );
        }
    }
}

/// The L4 pair's report block — the aggregation counterpart of
/// [`print_indicative_report`]'s per-pair section. An aggregation renders
/// no lines and acquires no template map (RFC 0002 §6.5: the aggregate
/// path's `materialize_bytes_read`/`registry_bytes_read` are structurally
/// zero, see [`ourios_bench::OuriosAggregateAnswer`]), so there is no
/// count-scan/materialize/registry breakdown to print, and this slice
/// does not wire an L4 latency channel (§7 DEFERRED already covers the
/// bytes verdict; latency is corroborating-only everywhere else too, so
/// this is a scope boundary, not a gap). Reuses [`print_pair_bytes_gates`]
/// for the shared bytes-channel labeling.
fn print_l4_report(
    spec: &PairSpec,
    ourios_bytes: u64,
    loki_fetched: &LokiFetchedBytes,
    loki_processed: u64,
) {
    let loki_storage = loki_fetched.compressed_bytes + loki_fetched.head_chunk_bytes;
    println!("--- pair [{}] rows={} ---", spec.label, spec.expected_rows);
    println!("dsl: {}", spec.dsl);
    println!("logql: {}", spec.logql);
    println!("ourios bytes_read (grouped-count scan, compressed, fetched) = {ourios_bytes}");
    println!(
        "loki   storage-side bytes (conservative)  = {loki_storage} \
         (compressed={} + head_chunk={})",
        loki_fetched.compressed_bytes, loki_fetched.head_chunk_bytes,
    );
    println!("loki   totalBytesProcessed (decompressed) = {loki_processed}");
    print_pair_bytes_gates(spec, ourios_bytes, loki_storage, loki_processed);
}

/// Measure, equivalence-check, and report the L4 pair — the aggregation
/// counterpart of the line-returning pairs' report loop, kept separate
/// (see `rfc0031_indicative_comparative_run`'s call-site comment) rather
/// than folded into `OuriosMeasured`/`Measured`, which are built around
/// `LineKey` multisets. `l4_loki` is the Loki-side matrix measurement
/// already gathered inside the same container session the other pairs
/// share (the dispatch run's async block). Called LAST, after the
/// L1–L3/L6 evidence has printed and their frozen gates have asserted,
/// so an L4-only failure cannot destroy that evidence (the same run #11
/// salvage lesson `loki_measure_pair`'s error-return, rather than panic,
/// already follows). Equivalence is never optional (RFC0031.1 applies to
/// every class, must-win or not); `M_L4` itself stays §7-DEFERRED —
/// [`print_l4_report`] prints both bytes channels' ratio with no verdict,
/// exactly like the fixture-level proof
/// (`rfc0031_5_l4_frequency_aggregation_bytes`).
/// Measure, equivalence-check, and report the L4 pair. A Loki-side
/// measurement failure (timeout/flake — the SAME failure mode
/// [`split_measurements`] salvages for L1–L3/L6) is pushed onto
/// `failures` and the pair is skipped, exactly like a flaky L1–L3/L6
/// pair never reaching `ok_specs`. A genuine equivalence MISMATCH
/// (both sides measured, but disagree) still hard-panics immediately
/// — RFC0031.1 equivalence is never optional, matching the L1–L3/L6
/// `compare_lines` assertion this mirrors — the failure modes are
/// deliberately not symmetric: a flake is salvageable, a mismatch is
/// not.
fn run_l4_pair(
    bucket_root: &std::path::Path,
    tenant: &TenantId,
    spec: &PairSpec,
    l4_loki: Result<L4Measured, String>,
    failures: &mut Vec<String>,
) {
    let (loki_groups, loki_processed, loki_fetched) = match l4_loki {
        Ok(measured) => measured,
        Err(detail) => {
            failures.push(format!(
                "L4 pair [{}] failed to measure: {detail}",
                spec.label
            ));
            return;
        }
    };
    let ourios_answer =
        ourios_aggregate_answer(bucket_root, tenant, &spec.dsl, spec.now, spec.window)
            .expect("l4 ourios aggregate answer");
    let outcome = compare_aggregations(&ourios_answer.groups, &loki_groups, 8);
    assert!(
        outcome.is_equal(),
        "RFC0031.5 — the two systems' grouped-count maps must be identical on [{}]: {outcome:?}",
        spec.label,
    );
    print_l4_report(
        spec,
        ourios_answer.bytes_read,
        &loki_fetched,
        loki_processed,
    );
}

/// The indicative run's report block, one section per pair, labeled per
/// the §7 partial freeze (2026-07-13) — the bytes-channel labeling
/// itself is [`print_pair_bytes_gates`].
///
/// The latency lines carry the §3.6 corroborating channel: warm p50s
/// (median of [`LATENCY_REPS`] post-poll reps — see the asymmetry note
/// on [`ourios_latency_p50`]: Ourios in-process, Loki over localhost
/// HTTP), the `loki_p50/ourios_p50` ratio in the bytes gates'
/// above-1.0-means-Ourios-faster orientation, and — for Floor pairs — the
/// RFC0031.7 latency floor as written (`ourios_p50 ≤ F_L6 × loki_p50`).
fn print_indicative_report(
    corpus_dir: &std::path::Path,
    total_records: u64,
    specs: &[PairSpec],
    ourios: &[OuriosMeasured],
    loki: &[Measured],
) {
    println!("=== RFC 0031 indicative comparative run ===");
    println!("corpus: {} ({total_records} records)", corpus_dir.display());
    for ((spec, ours), (_, loki_processed, loki_fetched, loki_latency)) in
        specs.iter().zip(ourios).zip(loki)
    {
        let answer = &ours.answer;
        let loki_storage = loki_fetched.compressed_bytes + loki_fetched.head_chunk_bytes;
        println!("--- pair [{}] rows={} ---", spec.label, spec.expected_rows);
        println!("dsl: {}", spec.dsl);
        // The Ourios figure is the honest TOTAL (§3.6 measurement-fidelity
        // amendment, 2026-07-12): count scan + row materialization +
        // template-map acquisition (RFC 0033: cold audit fold or warm
        // artifact GET) — the gates below ratio against it.
        println!(
            "ourios bytes_read (compressed, fetched)   = {} \
             (count_scan={} + materialize={} + registry={})",
            answer.bytes_read,
            answer.count_scan_bytes,
            answer.materialize_bytes,
            answer.registry_bytes,
        );
        // The registry component's RFC 0033 acquisition + publish
        // outcome (RFC0033.6's channel, §3.2 amendment): the artifact
        // persists across pairs by design, so a successful publish makes
        // the cold audit fold a once-per-run cost — while an abstention
        // (printed with its would-be size vs the folded bytes) leaves
        // every pair cold, run #20's finding.
        println!(
            "ourios template-map acquisition (RFC 0033) = {}",
            ours.template_map
        );
        println!(
            "loki   storage-side bytes (conservative)  = {loki_storage} \
             (compressed={} + head_chunk={})",
            loki_fetched.compressed_bytes, loki_fetched.head_chunk_bytes,
        );
        println!("loki   totalBytesProcessed (decompressed) = {loki_processed}");
        print_pair_bytes_gates(spec, answer.bytes_read, loki_storage, *loki_processed);
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        match ours.latency_p50 {
            Some(p50) => println!(
                "ourios latency_p50 (warm, in-process)      = {:.3} ms ({LATENCY_REPS} reps)",
                ms(p50),
            ),
            None => println!("ourios latency_p50                         = unmeasured"),
        }
        match loki_latency {
            Some(p50) => println!(
                "loki   latency_p50 (warm, localhost HTTP)  = {:.3} ms ({LATENCY_REPS} reps)",
                ms(*p50),
            ),
            None => println!("loki   latency_p50                         = unmeasured"),
        }
        if let (Some(ours_p50), Some(loki_p50)) = (ours.latency_p50, *loki_latency) {
            // A zero p50 (timer resolution coarser than the query) would
            // print inf/NaN — say "undefined" instead; the floor gate
            // below already treats zero as Invalid.
            if ours_p50.is_zero() || loki_p50.is_zero() {
                println!(
                    "latency ratio loki_p50/ourios_p50 = undefined (a p50 of 0 — \
                     timer resolution coarser than the query)"
                );
            } else {
                println!(
                    "latency ratio loki_p50/ourios_p50 = {:.2} (>1.0 = Ourios faster; \
                     corroborating only — Ourios is timed in-process, Loki over \
                     localhost HTTP, which favours Ourios on sub-millisecond answers)",
                    ms(loki_p50) / ms(ours_p50),
                );
            }
            match spec.class {
                PairClass::WindowFloor => println!(
                    "latency floor gate (RFC0031.7 — §7 FROZEN, floor factor {}): {:?}",
                    spec.margin,
                    latency_floor_gate(ours_p50, loki_p50, spec.margin),
                ),
                PairClass::Diagnostic => println!(
                    "latency floor (diagnostic reference, factor {}): {:?}",
                    spec.margin,
                    latency_floor_gate(ours_p50, loki_p50, spec.margin),
                ),
                PairClass::L1 | PairClass::L2 | PairClass::L3 | PairClass::L4 => {}
            }
        }
    }
}

#[test]
fn pick_selective_pair_finds_the_fixture_error_row() {
    // The picker is locally provable on the shared fixture: one ERROR row,
    // text-consistent, in the comparative-fixture service.
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let pair = pick_selective_pair(corpus.path());
    assert_eq!(
        pair.service, FIXTURE_SERVICE,
        "the (1, 17) tie against FIXTURE_SERVICE_B's ERROR row breaks to \
         the lexicographically smaller service"
    );
    assert_eq!(
        pair.threshold, 17,
        "the ERROR band is the rarest single-text band"
    );
    assert_eq!(pair.text, "ERROR");
    assert_eq!(pair.rows, 1, "exactly the one FIXTURE_SERVICE ERROR record");
    assert_eq!(pair.total_records, 4);
    assert_eq!(pair.min_ts, 1_000_000);
    assert_eq!(pair.max_ts, 1_003_000, "the service-B record is the latest");
}

#[test]
fn pick_selective_pair_generalizes_without_error_rows() {
    // The otel-demo-v8 shape in miniature: an INFO-dominated service with
    // a rare WARN band and NO ERROR rows anywhere — the generalization
    // path run #1 surfaced. The picker must select the WARN band.
    let records = vec![
        FixtureRecord {
            time_unix_nano: 1_000,
            severity_number: 9,
            severity_text: "INFO",
            body: "user 1 logged in",
            trace_id: None,
            service: FIXTURE_SERVICE,
        },
        FixtureRecord {
            time_unix_nano: 2_000,
            severity_number: 9,
            severity_text: "INFO",
            body: "user 2 logged in",
            trace_id: None,
            service: FIXTURE_SERVICE,
        },
        FixtureRecord {
            time_unix_nano: 3_000,
            severity_number: 13,
            severity_text: "WARN",
            body: "cache nearly full",
            trace_id: None,
            service: FIXTURE_SERVICE,
        },
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let pair = pick_selective_pair(corpus.path());
    assert_eq!(pair.service, FIXTURE_SERVICE);
    assert_eq!(
        pair.threshold, 13,
        "the WARN band is the rarest single-text band"
    );
    assert_eq!(pair.text, "WARN");
    assert_eq!(pair.rows, 1);
}

#[test]
fn pick_window_pair_prefers_a_clean_mid_corpus_window() {
    let ts: Vec<u64> = (0..100).map(|i| 1_000 + i * 10).collect();
    let (a, b) = pick_window_pair(&ts, &[], 10).expect("every window is clean");
    assert_eq!(a, 1_000 + 45 * 10, "centred start index (100−10)/2 = 45");
    assert_eq!(b, 1_000 + 54 * 10 + 1, "b = last in-window timestamp + 1");
    assert_eq!(ts.iter().filter(|&&t| t >= a && t < b).count(), 10);
}

#[test]
fn pick_window_pair_edges_are_clean() {
    // Duplicates and 1 ns gaps around the centre force the picker off the
    // centred window; whatever it returns must satisfy both edge
    // invariants and contain exactly k rows.
    let ts: Vec<u64> = vec![0, 10, 20, 30, 40, 40, 41, 50, 60, 70, 80, 90];
    let k = 3;
    let (a, b) = pick_window_pair(&ts, &[], k).expect("clean windows exist");
    assert_eq!(ts.iter().filter(|&&t| t >= a && t < b).count(), k);
    assert!(
        !ts.contains(&b),
        "end-inclusive semantics at b must not admit an extra row"
    );
    let first_inside = ts.iter().position(|&t| t >= a).expect("window non-empty");
    assert!(
        first_inside == 0 || ts[first_inside - 1] < a,
        "no earlier record shares the window's start timestamp"
    );
}

#[test]
fn pick_window_pair_avoids_poison_timestamps() {
    let ts: Vec<u64> = (0..100).map(|i| i * 10).collect();
    let poison = vec![455];
    let (a, b) = pick_window_pair(&ts, &poison, 10).expect("clean windows exist off-centre");
    assert!(
        !(a..b).contains(&455),
        "a zero-time row's fallback timestamp inside the window guarantees \
         an equivalence mismatch"
    );
    assert_eq!(ts.iter().filter(|&&t| t >= a && t < b).count(), 10);
}

#[test]
fn pick_window_pair_none_when_insufficient() {
    assert_eq!(pick_window_pair(&[1, 2, 3], &[], 4), None);
    assert_eq!(pick_window_pair(&[1, 2, 3], &[], 0), None);
}

#[test]
fn collect_service_timestamps_reads_the_fixture() {
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let (clean, poison) = collect_service_timestamps(corpus.path(), FIXTURE_SERVICE);
    assert_eq!(clean, vec![1_000_000, 1_001_000, 1_002_000]);
    assert!(poison.is_empty(), "the fixture has no zero-time records");
    let (other_clean, other_poison) = collect_service_timestamps(corpus.path(), "no-such-service");
    assert!(other_clean.is_empty() && other_poison.is_empty());
}

/// Locally proves the window-pair query shape end to end on the fixture:
/// the bare-service DSL parses, and the `now = end` / `window = end −
/// start` mapping slices exactly `[start, end)` (the querier's
/// `time_window_filter` is `ts ≥ now − window ∧ ts < now`).
#[test]
fn window_pair_dsl_slices_the_fixture() {
    let records = comparative_fixture(1_000_000);
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

    // [1_001_000, 1_002_001) → the second and third fixture records.
    let (start, end) = (1_001_000u64, 1_002_001u64);
    let dsl = format!("service == \"{FIXTURE_SERVICE}\" | limit 5000");
    let answer = ourios_bench::ourios_query_answer(bucket.path(), &tenant, &dsl, end, end - start)
        .expect("ourios answer");
    let mut got: Vec<u64> = answer
        .lines
        .iter()
        .map(|k| k.timestamp_unix_nanos)
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![1_001_000, 1_002_000]);
}

// ---------------------------------------------------------------------------
// Promoted `service.name` parity (RFC 0022 §3.1 in the comparative store).
//
// Loki's stock OTLP ingest promotes `service.name` to the `service_name`
// stream label by default, so every service-scoped LogQL query in the
// comparison is label-indexed. Ourios holds the same ground by
// construction: RFC 0022 §3.1 promotes `service.name` implicitly on every
// writer constructor, so the comparative store's files carry a
// stats-bearing `resource.service.name` column and the DSL `service ==`
// predicate compiles to the promoted arm (RFC 0022 §3.3), not the
// JSON-substring fallback. These tests pin that parity: a writer or
// store-builder change that demoted the column would silently hold Ourios
// to a harder version of the service-scoped question than Loki answers,
// exactly the strawman RFC 0031 §2 forbids in either direction.
// ---------------------------------------------------------------------------

/// One OTLP `LogsData` line for `service`, one INFO record per
/// `(time_unix_nano, body)` pair — the hand-rolled two-service analogue
/// of `fixture_logs_data` (which is fixed to the single
/// [`FIXTURE_SERVICE`] resource).
fn service_logs_data(
    service: &str,
    records: &[(u64, &str)],
) -> opentelemetry_proto::tonic::logs::v1::LogsData {
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, LogsData, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;

    let log_records = records
        .iter()
        .map(|&(ts, body)| LogRecord {
            time_unix_nano: ts,
            severity_number: 9,
            severity_text: "INFO".to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(body.to_string())),
            }),
            ..LogRecord::default()
        })
        .collect();
    LogsData {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue(service.to_string())),
                    }),
                    ..KeyValue::default()
                }],
                ..Resource::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records,
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    }
}

/// A fixed in-range instant for the two-service corpus (2026-04-02 UTC;
/// local tests don't face Loki's reject-old-samples window, so any base
/// works — fixed for deterministic partition paths).
const TWO_SERVICE_BASE_NS: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;

/// The `(timestamp, body)` records each service of the two-service
/// corpus carries. `svc-a` sits two hour-partitions before `svc-b`, so
/// each service lands in its own file with service-homogeneous row-group
/// stats — the layout a `service ==` predicate can actually prune
/// against (a row group holding both services has min/max spanning both,
/// and nothing to skip).
fn two_service_records(service: &str) -> Vec<(u64, &'static str)> {
    let (base, bodies): (u64, [&'static str; 3]) = match service {
        "svc-a" => (
            TWO_SERVICE_BASE_NS,
            ["alpha one", "alpha two", "alpha three"],
        ),
        "svc-b" => (
            TWO_SERVICE_BASE_NS + 2 * HOUR_NS,
            ["beta one", "beta two", "beta three"],
        ),
        other => panic!("two_service_records knows svc-a and svc-b, not {other:?}"),
    };
    bodies
        .into_iter()
        .zip([0u64, 1_000, 2_000])
        .map(|(body, off)| (base + off, body))
        .collect()
}

/// Build the registry-bearing comparative store over the two-service
/// corpus. Returns the live bucket dir plus the build summary.
fn build_two_service_store() -> (tempfile::TempDir, ourios_bench::BuiltStore) {
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    let jsonl = ["svc-a", "svc-b"]
        .map(|svc| {
            serde_json::to_string(&service_logs_data(svc, &two_service_records(svc)))
                .expect("serialize LogsData")
        })
        .join("\n");
    std::fs::write(corpus.path().join("two-service.jsonl"), jsonl).expect("write corpus");

    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    (bucket, built)
}

/// Every data file the comparative store publishes carries the RFC 0022
/// promoted `resource.service.name` column, byte-for-byte the field the
/// writer's default (implicit-`service.name`) schema declares.
#[test]
fn comparative_store_promotes_the_service_name_column() {
    let (bucket, built) = build_two_service_store();
    assert_eq!(built.rows, 6);
    assert_eq!(built.files, 2, "one file per hour partition");

    let expected_schema =
        ourios_parquet::data_schema_with_promoted(&ourios_parquet::PromotedAttributes::default());
    let expected_field = expected_schema
        .field_with_name("resource.service.name")
        .expect("default promoted schema declares the service column");

    let mut checked = 0;
    let mut stack = vec![bucket.path().join("data")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                let file = std::fs::File::open(&path).expect("open parquet file");
                let builder =
                    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
                        .expect("parquet footer");
                let field = builder
                    .schema()
                    .field_with_name("resource.service.name")
                    .unwrap_or_else(|_| {
                        panic!("{} lacks the promoted service column", path.display())
                    });
                assert_eq!(field, expected_field, "{}", path.display());
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 2, "both data files checked");
}

/// A `service ==` query over the two-service store answers off the
/// promoted column: the other service's row group is pruned by its
/// statistics, and the query reads strictly fewer bytes than the
/// full-window scan. The JSON-substring fallback arm can do neither —
/// this is the pruning evidence RFC 0031's service-scoped pairs rest on.
#[test]
fn service_predicate_prunes_on_the_promoted_column() {
    let (bucket, built) = build_two_service_store();
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    let query = ourios_querier::dsl::parse("service == \"svc-a\" | limit 100").expect("parse DSL");
    let querier = ourios_querier::Querier::new(bucket.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = runtime
        .block_on(querier.run_query(&query, &tenant, now, window, None))
        .expect("service query");
    assert_eq!(result.rows, 3, "exactly the svc-a rows match");
    assert!(
        result.stats.row_groups_pruned >= 1,
        "svc-b's row group must be pruned via promoted-column statistics \
         (scanned {}, pruned {})",
        result.stats.row_groups_scanned,
        result.stats.row_groups_pruned,
    );

    // The same two queries through the comparative measurement channel:
    // the service-scoped answer is the right rows AND fewer bytes than
    // the full-window scan.
    let service_answer = ourios_bench::ourios_query_answer(
        bucket.path(),
        &tenant,
        "service == \"svc-a\" | limit 100",
        now,
        window,
    )
    .expect("service answer");
    let expected: Vec<LineKey> = two_service_records("svc-a")
        .into_iter()
        .map(|(ts, body)| LineKey {
            timestamp_unix_nanos: ts,
            body: body.as_bytes().to_vec(),
        })
        .collect();
    assert!(
        compare_lines(&service_answer.lines, &expected, 8).is_equal(),
        "the promoted arm must return exactly the svc-a lines",
    );

    let full_answer = ourios_bench::ourios_query_answer(
        bucket.path(),
        &tenant,
        "severity >= 0 | limit 100",
        now,
        window,
    )
    .expect("full-window answer");
    assert_eq!(
        full_answer.lines.len(),
        6,
        "the full scan reads both services"
    );
    assert!(
        service_answer.bytes_read < full_answer.bytes_read,
        "service-scoped bytes_read ({}) must undercut the full scan ({})",
        service_answer.bytes_read,
        full_answer.bytes_read,
    );
}

#[test]
fn select_pair_candidates_rejects_poisoned_bands() {
    use std::collections::HashMap;

    let mut bands: SeverityBands = HashMap::new();
    bands.entry(9).or_default().insert("INFO".into(), 5);
    bands.entry(13).or_default().insert("WARN".into(), 1);

    let clean: HashMap<String, (SeverityBands, SeverityBands)> =
        [("svc".to_string(), (bands.clone(), HashMap::new()))].into();
    assert!(
        select_pair_candidates(&clean)
            .iter()
            .any(|&(rows, threshold, _, text)| rows == 1 && threshold == 13 && text == "WARN"),
        "without poison the WARN band is a candidate"
    );

    // A zero-time row the DSL side would select (number ≥ threshold)…
    let mut poison_high: SeverityBands = HashMap::new();
    poison_high.entry(17).or_default().insert("ERROR".into(), 1);
    let poisoned: HashMap<String, (SeverityBands, SeverityBands)> =
        [("svc".to_string(), (bands.clone(), poison_high))].into();
    assert!(
        select_pair_candidates(&poisoned).is_empty(),
        "a poison row at number ≥ threshold disqualifies the candidate"
    );

    // …and one the LogQL side would select (same text, below threshold).
    let mut poison_text: SeverityBands = HashMap::new();
    poison_text.entry(5).or_default().insert("WARN".into(), 1);
    let poisoned_text: HashMap<String, (SeverityBands, SeverityBands)> =
        [("svc".to_string(), (bands, poison_text))].into();
    assert!(
        select_pair_candidates(&poisoned_text).is_empty(),
        "a below-threshold poison row carrying the text disqualifies the candidate"
    );
}

#[test]
fn pick_trace_pair_finds_the_shared_fixture_trace() {
    // Three fixture records share FIXTURE_TRACE across two services
    // (eligible: 3 rows, clean timestamps, named services); the remaining
    // record's trace has only one row — below the 2-row floor — so
    // exactly one candidate remains.
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let (hex, rows) = pick_trace_pair(corpus.path()).expect("the shared trace is eligible");
    assert_eq!(hex, FIXTURE_TRACE);
    assert_eq!(rows, 3, "two FIXTURE_SERVICE rows + the service-B row");
}

#[test]
fn pick_trace_pair_rejects_zero_ts_traces() {
    // A trace containing any zero-time_unix_nano row is poison (the two
    // systems answer with different keys); with every trace poisoned or
    // single-row, there is no candidate.
    let records = vec![
        FixtureRecord {
            time_unix_nano: 0,
            severity_number: 9,
            severity_text: "INFO",
            body: "zero ts row",
            trace_id: Some("00112233445566778899aabbccddeeff"),
            service: FIXTURE_SERVICE,
        },
        FixtureRecord {
            time_unix_nano: 1_000,
            severity_number: 9,
            severity_text: "INFO",
            body: "clean row same trace",
            trace_id: Some("00112233445566778899aabbccddeeff"),
            service: FIXTURE_SERVICE,
        },
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    assert_eq!(pick_trace_pair(corpus.path()), None);
}

#[test]
fn pick_trace_pair_prefers_multi_service_then_smallest_id() {
    let rec = |ts, body, trace, service| FixtureRecord {
        time_unix_nano: ts,
        severity_number: 9,
        severity_text: "INFO",
        body,
        trace_id: Some(trace),
        service,
    };

    // A 2-row multi-service trace beats a 3-row single-service one.
    let records = vec![
        rec(
            1_000,
            "big a",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
        rec(
            2_000,
            "big b",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
        rec(
            3_000,
            "big c",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
        rec(
            4_000,
            "spans a",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            FIXTURE_SERVICE,
        ),
        rec(
            5_000,
            "spans b",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            FIXTURE_SERVICE_B,
        ),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");
    assert_eq!(
        pick_trace_pair(corpus.path()),
        Some(("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(), 2)),
        "multi-service wins over more rows"
    );

    // Otherwise-equal candidates tie-break to the lexicographically
    // smallest id.
    let records = vec![
        rec(
            1_000,
            "tie c1",
            "cccccccccccccccccccccccccccccccc",
            FIXTURE_SERVICE,
        ),
        rec(
            2_000,
            "tie c2",
            "cccccccccccccccccccccccccccccccc",
            FIXTURE_SERVICE,
        ),
        rec(
            3_000,
            "tie a1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
        rec(
            4_000,
            "tie a2",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");
    assert_eq!(
        pick_trace_pair(corpus.path()),
        Some(("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), 2)),
        "equal candidates pick the smallest id"
    );
}

#[test]
fn template_needle_takes_the_longest_safe_constant_run() {
    let fixed = |s: &str| OwnedToken::Fixed(s.to_owned());

    // Wildcards and unsafe tokens split runs; the longest surviving run
    // wins, joined with single spaces.
    let tokens = vec![
        fixed("shutting"),
        fixed("down"),
        OwnedToken::Wildcard,
        fixed("connection"),
        fixed("established"),
        fixed("to"),
        fixed("peer"),
        fixed("via:"), // unsafe ':' splits the run
        fixed("gateway"),
    ];
    assert_eq!(
        template_needle(&tokens).as_deref(),
        Some("connection established to peer"),
    );

    // Under 10 chars → no candidate; all wildcards → no candidate.
    assert_eq!(template_needle(&[fixed("logged"), fixed("in")]), None);
    assert_eq!(
        template_needle(&[OwnedToken::Wildcard, OwnedToken::Wildcard]),
        None,
    );

    // A length tie breaks to the lexicographically smallest run.
    let tie = vec![
        fixed("bbbbb"),
        fixed("bbbb"),
        OwnedToken::Wildcard,
        fixed("aaaaa"),
        fixed("aaaa"),
    ];
    assert_eq!(template_needle(&tie).as_deref(), Some("aaaaa aaaa"));
}

#[test]
fn eligible_template_candidates_enforce_poison_rules_and_bounds() {
    let candidates: Vec<(u64, String)> = [
        (1, "needle alpha"),         // clean, 3 matches
        (2, "needle beta"),          // zero-ts poison
        (3, "needle gamma"),         // empty-service poison
        (4, "needle delta"),         // 1 match — under the 2-row floor
        (5, "needle epsilon"),       // 4001 — over the one-page cap
        (6, "short need"),           // clean, 2 matches — most selective
        (7, "a longer needle here"), // clean, 3 matches, longer needle
    ]
    .into_iter()
    .map(|(id, needle)| (id, needle.to_string()))
    .collect();
    let tally = |matches, has_zero_ts, has_empty_service| NeedleTally {
        matches,
        has_zero_ts,
        has_empty_service,
    };
    let tallies = vec![
        tally(3, false, false),
        tally(3, true, false),
        tally(3, false, true),
        tally(1, false, false),
        tally(4001, false, false),
        tally(2, false, false),
        tally(3, false, false),
    ];

    assert_eq!(
        eligible_template_candidates(&candidates, &tallies),
        vec![
            (6, "short need", 2),           // fewest matches first
            (7, "a longer needle here", 3), // then the longer needle
            (1, "needle alpha", 3),
        ],
        "poisoned and out-of-bounds candidates drop; the rest sort \
         most-selective first with deterministic tiebreaks",
    );
}

#[test]
fn pick_template_pair_finds_a_validated_needle() {
    // Three rows of one template whose constant run is ≥ 10 safe chars
    // ("connection established to peer" — the numbers mask to one
    // wildcard slot) plus a second template whose longest run ("logged
    // in", 9 chars) is under the floor: exactly one candidate, and it
    // validates — 3 needle-matching corpus lines == 3 template rows,
    // every rendered row contains the needle.
    let records: Vec<(u64, &str)> = vec![
        (1_000, "connection established to peer 10"),
        (2_000, "connection established to peer 11"),
        (3_000, "connection established to peer 12"),
        (4_000, "user 4 logged in"),
        (5_000, "user 5 logged in"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
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

    let pair = pick_template_pair(corpus.path(), bucket.path(), &tenant, now, window)
        .expect("the peer template validates");
    assert_eq!(pair.needle, "connection established to peer");
    assert_eq!(pair.rows, 3, "exactly the three peer rows");
    assert_ne!(pair.template_id, 0, "never the NO_TEMPLATE sentinel");
}

#[test]
fn pick_template_pair_rejects_substring_collisions() {
    // Two rows of the peer template, plus ONE line of a DIFFERENT
    // template (different token count) that contains the same constant
    // text as a substring: the needle matches 3 corpus lines against 2
    // template rows, so count-equality fails — a Loki line filter for it
    // would return a row the DSL side never selects. The colliding
    // template's own needle is the same string (3 matches vs 1 row), so
    // no candidate validates and the picker returns None.
    let records: Vec<(u64, &str)> = vec![
        (1_000, "connection established to peer 10"),
        (2_000, "connection established to peer 11"),
        (3_000, "retry: connection established to peer 12 via proxy"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
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

    assert!(
        pick_template_pair(corpus.path(), bucket.path(), &tenant, now, window).is_none(),
        "a needle whose corpus count exceeds the template's rows must be rejected",
    );
}

#[test]
fn pick_bucket_width_targets_four_buckets_floored_at_one_second() {
    assert_eq!(pick_bucket_width(4_000_000_000), "1s", "4s span / 4 = 1s");
    assert_eq!(
        pick_bucket_width(400_000_000),
        "1s",
        "a sub-second span floors to the DSL's finest unit",
    );
    assert_eq!(
        pick_bucket_width(4 * 60_000_000_000),
        "1m",
        "4m span / 4 = 1m"
    );
    assert_eq!(
        pick_bucket_width(4 * 3_600_000_000_000),
        "1h",
        "4h span / 4 = 1h"
    );
    assert_eq!(
        pick_bucket_width(4 * 86_400_000_000_000),
        "1d",
        "4d span / 4 = 1d"
    );
    assert_eq!(
        pick_bucket_width(4 * 7 * 86_400_000_000_000),
        "1w",
        "4w span / 4 = 1w",
    );
    assert_eq!(
        pick_bucket_width(9 * 60_000_000_000),
        "2m",
        "9m / 4 = 2.25m, floored to 2m",
    );
}

#[test]
fn bucket_width_seconds_converts_every_dsl_unit() {
    assert_eq!(bucket_width_seconds("30s"), 30);
    assert_eq!(bucket_width_seconds("2m"), 120);
    assert_eq!(bucket_width_seconds("1h"), 3_600);
    assert_eq!(bucket_width_seconds("1d"), 86_400);
    assert_eq!(bucket_width_seconds("1w"), 7 * 86_400);
}

#[test]
fn regex_escape_escapes_go_re2_metacharacters() {
    assert_eq!(regex_escape("peer"), "peer");
    assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
    assert_eq!(regex_escape(r"[x](y)"), r"\[x\]\(y\)");
}

#[test]
fn param_capture_regex_anchors_on_the_nearest_fixed_neighbours() {
    let fixed = |s: &str| OwnedToken::Fixed(s.to_owned());

    // A single trailing wildcard: only a prefix neighbour exists.
    let tokens = vec![
        fixed("connection"),
        fixed("established"),
        fixed("to"),
        fixed("peer"),
        OwnedToken::Wildcard,
    ];
    assert_eq!(
        param_capture_regex(&tokens, 0).as_deref(),
        Some(r"peer\s+(?P<value>\S+)"),
    );

    // A wildcard with fixed neighbours on both sides.
    let tokens = vec![
        fixed("user"),
        OwnedToken::Wildcard,
        fixed("logged"),
        fixed("in"),
    ];
    assert_eq!(
        param_capture_regex(&tokens, 0).as_deref(),
        Some(r"user\s+(?P<value>\S+)\s+logged"),
    );

    // The SECOND wildcard, disambiguated from the first by index.
    let tokens = vec![
        fixed("from"),
        OwnedToken::Wildcard,
        fixed("to"),
        OwnedToken::Wildcard,
        fixed("bytes"),
    ];
    assert_eq!(
        param_capture_regex(&tokens, 1).as_deref(),
        Some(r"to\s+(?P<value>\S+)\s+bytes"),
    );

    // A metacharacter neighbour is escaped.
    let tokens = vec![fixed("count."), OwnedToken::Wildcard];
    assert_eq!(
        param_capture_regex(&tokens, 0).as_deref(),
        Some(r"count\.\s+(?P<value>\S+)"),
    );

    // Out of range: no such wildcard position.
    assert_eq!(
        param_capture_regex(&[fixed("no"), fixed("wildcards")], 0),
        None,
    );
    assert_eq!(param_capture_regex(&[OwnedToken::Wildcard], 1), None);

    // Unanchored: neither neighbour is fixed — a single wildcard with no
    // surrounding tokens, and two adjacent wildcards where the target's
    // neighbour on both sides is itself a wildcard. An unanchored
    // `(?P<value>\S+)` would match any token, so both must reject rather
    // than return a pattern a caller could unknowingly measure with.
    assert_eq!(param_capture_regex(&[OwnedToken::Wildcard], 0), None);
    assert_eq!(
        param_capture_regex(&[OwnedToken::Wildcard, OwnedToken::Wildcard], 0),
        None,
    );
    assert_eq!(
        param_capture_regex(&[OwnedToken::Wildcard, OwnedToken::Wildcard], 1),
        None,
    );
}

#[test]
fn pick_frequency_pair_finds_a_moderate_cardinality_group() {
    // Two ids ("10"/"11") of the peer template, spread over four
    // one-second buckets — cardinality 2 (within 2..=50) and 6 total
    // rows (above the L4_MIN_ROWS floor).
    let records: Vec<(u64, &str)> = vec![
        (100_000_000, "connection established to peer 10"),
        (500_000_000, "connection established to peer 10"),
        (900_000_000, "connection established to peer 11"),
        (2_100_000_000, "connection established to peer 10"),
        (2_500_000_000, "connection established to peer 11"),
        (3_000_000_000, "connection established to peer 11"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
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

    let pair = pick_frequency_pair(bucket.path(), &tenant, now, window)
        .expect("the peer template's trailing id validates");
    assert_eq!(pair.param, 0);
    assert_eq!(pair.bucket_width, "1s");
    assert_eq!(pair.needle, "connection established to peer");
    assert_eq!(pair.capture_regex, r"peer\s+(?P<value>\S+)");
    assert_eq!(pair.groups.values().sum::<u64>(), 6);
    assert!(pair.bytes_read > 0);
}

#[test]
fn pick_frequency_pair_rejects_below_the_row_floor() {
    // Three rows, each a DISTINCT trailing id (cardinality 3, inside the
    // 2..=50 band) but under L4_MIN_ROWS (4): the same generalised bound
    // that rejects kafka's per-line-unique slots at scale — small enough
    // here to pin the row floor explicitly. No other template exists to
    // fall through to, so the picker returns None loudly rather than
    // silently degrading to a weaker candidate.
    let records: Vec<(u64, &str)> = vec![
        (1_000, "connection established to peer 10"),
        (2_000, "connection established to peer 11"),
        (3_000, "connection established to peer 12"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
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

    assert!(
        pick_frequency_pair(bucket.path(), &tenant, now, window).is_none(),
        "3 rows is below L4_MIN_ROWS — the picker must reject loudly, not fall through",
    );
}

#[test]
fn frequency_shape_rejection_enforces_the_row_ceiling() {
    // A ~971K-row candidate (a service's dominant, near-catch-all
    // template) is exactly the run #3 dispatch finding: cardinality 2
    // and 2 bucket windows both clear their floors, but the total row
    // count made the Loki poll unable to finish within its shared
    // 300s deadline. Two groups summing past L4_MAX_ROWS reproduces
    // the shape without building a 100K+-row real corpus fixture.
    let groups = HashMap::from([
        (
            AggKey {
                bucket_start_unix_nanos: 0,
                group_key: "a".to_string(),
            },
            L4_MAX_ROWS / 2 + 1,
        ),
        (
            AggKey {
                bucket_start_unix_nanos: 1_000_000_000,
                group_key: "b".to_string(),
            },
            L4_MAX_ROWS / 2 + 1,
        ),
    ]);
    let rejection =
        frequency_shape_rejection(&groups).expect("a candidate past L4_MAX_ROWS must be rejected");
    assert!(
        rejection.contains(&L4_MAX_ROWS.to_string()),
        "the rejection reason must name the ceiling: {rejection}",
    );

    // One row under the ceiling passes (isolating the ceiling from the
    // other floors: still cardinality 2, still 2 bucket windows).
    let mut under_ceiling = groups;
    let cell = under_ceiling
        .get_mut(&AggKey {
            bucket_start_unix_nanos: 0,
            group_key: "a".to_string(),
        })
        .expect("cell exists");
    *cell -= 2;
    assert_eq!(
        frequency_shape_rejection(&under_ceiling),
        None,
        "exactly at the ceiling must pass",
    );
}

fn synthetic_frequency_pair(capture_regex: &str) -> FrequencyPair {
    FrequencyPair {
        template_id: 1,
        param: 0,
        needle: "connection established to peer".to_string(),
        capture_regex: capture_regex.to_string(),
        bucket_width: "1s".to_string(),
        groups: HashMap::from([(
            AggKey {
                bucket_start_unix_nanos: 0,
                group_key: "10".to_string(),
            },
            1,
        )]),
        bytes_read: 1,
    }
}

#[test]
fn l4_pair_spec_uses_a_backtick_delimited_regexp_argument() {
    // The bug this guards: a double-quoted LogQL string literal
    // interprets `capture_regex`'s own Go RE2 escapes (`\s+`) as ITS
    // OWN escape sequences, and `\s` is not a valid one — Loki's
    // parser rejects the query with "invalid char escape" before the
    // pattern reaches the regex engine (the exact failure the first L4
    // dispatch hit). Backticks pass the pattern through literally.
    let pair = synthetic_frequency_pair(r"peer\s+(?P<value>\S+)");
    let margins = ourios_bench::ComparativeMargins::default();
    let spec = l4_pair_spec(
        &pair,
        0,
        1_000_000_000,
        2_000_000_000,
        2_000_000_000,
        &margins,
    )
    .expect("a plain capture_regex carries no backtick");
    assert!(
        spec.logql.contains(r"regexp `peer\s+(?P<value>\S+)`"),
        "the regexp argument must be backtick-delimited: {}",
        spec.logql,
    );
}

#[test]
fn l4_pair_spec_rejects_a_capture_regex_containing_a_backtick() {
    // A backtick in the pattern (from an unescaped fixed-token
    // neighbour — `regex_escape` does not escape backticks, since they
    // are not an RE2 metacharacter) would prematurely close the
    // backtick-delimited raw string. Reject loudly rather than emit a
    // malformed query.
    let pair = synthetic_frequency_pair("peer`s\\s+(?P<value>\\S+)");
    let margins = ourios_bench::ComparativeMargins::default();
    assert!(
        l4_pair_spec(
            &pair,
            0,
            1_000_000_000,
            2_000_000_000,
            2_000_000_000,
            &margins
        )
        .is_none(),
        "a capture_regex containing a backtick must be rejected, not emitted",
    );
}

#[test]
fn pick_frequency_pair_rejects_a_single_bucket_window() {
    // Cardinality 2 and 4 rows both clear their respective floors, but
    // every timestamp falls inside the same 1s bucket window — the
    // candidate never exercises the bucket dimension of the (bucket,
    // group_key) → count equivalence shape, so the picker must reject
    // it rather than measure an L4 pair that leaves bucket alignment
    // untested.
    let records: Vec<(u64, &str)> = vec![
        (100_000_000, "connection established to peer 10"),
        (300_000_000, "connection established to peer 10"),
        (500_000_000, "connection established to peer 11"),
        (700_000_000, "connection established to peer 11"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
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

    assert!(
        pick_frequency_pair(bucket.path(), &tenant, now, window).is_none(),
        "all rows land in one bucket window — the picker must reject loudly, not fall through",
    );
}

#[test]
fn pick_frequency_pair_rejects_a_single_value_slot() {
    // Every row carries the SAME trailing id — cardinality 1, below the
    // L4 grouping floor of 2 (a single value is not a grouping question):
    // enough rows to clear L4_MIN_ROWS, isolating the cardinality bound
    // from the row-floor rejection the previous test pins.
    let records: Vec<(u64, &str)> = vec![
        (100_000_000, "connection established to peer 10"),
        (500_000_000, "connection established to peer 10"),
        (900_000_000, "connection established to peer 10"),
        (2_100_000_000, "connection established to peer 10"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
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

    assert!(
        pick_frequency_pair(bucket.path(), &tenant, now, window).is_none(),
        "cardinality 1 is below the L4 grouping floor — reject, don't fall through",
    );
}

#[test]
fn pick_rare_window_pair_selects_the_low_volume_service() {
    // Service B is rarer than FIXTURE_SERVICE (1 row vs 3) but a 1-row
    // window fails the 2-row floor, so the picker must fall through to
    // the next-rarest service with a clean window.
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let (service, start, end, rows) =
        pick_rare_window_pair(corpus.path(), "not-in-corpus").expect("a clean window exists");
    assert_eq!(
        service, FIXTURE_SERVICE,
        "the 1-row service B fails the 2-row floor; fall through"
    );

    // Excluding the only viable service yields a loud None, never a
    // duplicate of an existing pair (the run #16 lesson).
    assert_eq!(pick_rare_window_pair(corpus.path(), FIXTURE_SERVICE), None);
    assert_eq!(rows, 3, "all three FIXTURE_SERVICE rows fit one window");
    assert!(start < end);
    assert!(start >= 1_000_000 && end <= 1_002_001);
}

#[test]
fn median_duration_takes_the_middle_sample() {
    let ms = Duration::from_millis;

    // The K = 7 shape: the fourth-smallest, regardless of arrival order.
    let reps = [ms(70), ms(10), ms(50), ms(30), ms(20), ms(60), ms(40)];
    assert_eq!(median_duration(&reps), Some(ms(40)));

    assert_eq!(median_duration(&[ms(9), ms(1), ms(5)]), Some(ms(5)));
    assert_eq!(median_duration(&[ms(7)]), Some(ms(7)));
    assert_eq!(
        median_duration(&[ms(4), ms(1), ms(3), ms(2)]),
        Some(ms(2) + Duration::from_micros(500)),
        "even lengths take the mean of the two middles"
    );
    assert_eq!(median_duration(&[]), None);
}

#[test]
fn latency_floor_gate_decides_in_nanoseconds() {
    // The RFC0031.7 rule at F_L6 = 3: pass iff ourios_p50 ≤ 3 × loki_p50.
    let ms = Duration::from_millis;
    assert!(latency_floor_gate(ms(300), ms(100), 3).passed());
    assert!(!latency_floor_gate(ms(301), ms(100), 3).passed());

    // Sub-millisecond p50s stay decidable — the reason the gate runs on
    // nanosecond integers, not milliseconds.
    let us = Duration::from_micros;
    assert!(latency_floor_gate(us(300), us(100), 3).passed());
    assert!(!latency_floor_gate(us(301), us(100), 3).passed());

    // The shared gate's honesty guards carry over: a zero measurement is
    // Invalid, never a pass.
    assert!(!latency_floor_gate(Duration::ZERO, ms(100), 3).passed());
    assert!(matches!(
        latency_floor_gate(Duration::ZERO, ms(100), 3),
        ourios_bench::BytesGateOutcome::Invalid { .. }
    ));
}

#[test]
fn template_map_acquisition_gate_decides_warm_against_the_fold() {
    let no_refold = || -> Result<u64, String> { panic!("a measured cold fold must be used") };

    // No warm pair: nothing to decide (run #20's all-cold shape is
    // RFC 0033's own red, not this gate's) — and no refold is spent.
    assert_eq!(
        template_map_acquisition_failure(None, Some(513_862), no_refold),
        None,
    );

    // Warm with a cold pair's measured fold: pass at exactly warm × 2 ==
    // fold, fail one byte above — the ≤ 1/2 rule of the amended §5.6.
    assert_eq!(
        template_map_acquisition_failure(Some(250_000), Some(500_000), no_refold),
        None,
    );
    let over = template_map_acquisition_failure(Some(250_001), Some(500_000), no_refold);
    assert!(
        over.as_deref().is_some_and(|f| f.contains("RFC0033.6")),
        "{over:?}",
    );

    // The all-warm steady state (run #21) refolds once: the §9.15
    // record (187,904 B warm vs the 513,862 B fold, ≈ 1/2.73) passes.
    assert_eq!(
        template_map_acquisition_failure(Some(187_904), None, || Ok(513_862)),
        None,
    );
    assert!(template_map_acquisition_failure(Some(300_000), None, || Ok(513_862)).is_some());

    // A failed refold is non-evaluable — loud, never a spurious fail
    // (the RFC0031.7 unmeasured-latency stance).
    assert_eq!(
        template_map_acquisition_failure(Some(187_904), None, || Err("boom".to_string())),
        None,
    );

    // The lgates honesty rule: a zero measurement must not decide the
    // gate — least of all a 0-byte "warm" GET faking a pass.
    assert!(template_map_acquisition_failure(Some(0), Some(513_862), no_refold).is_some());
    assert!(template_map_acquisition_failure(Some(187_904), Some(0), no_refold).is_some());
}
