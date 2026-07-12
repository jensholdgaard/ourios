//! RFC 0031 — comparative evaluation vs Grafana Loki (§5 scenarios).
//!
//! One scenario per §5 acceptance criterion (RFC0031.1–.11). `.1`
//! (result-set equivalence) is **live**: a real Loki container run,
//! `#[ignore]`d because it needs Docker — the `loki-interop` CI job
//! executes it via `--ignored --exact` (the dex-oidc precedent). The
//! remaining ten are `#[ignore]`d red stubs, each discharged by its
//! named green slice.
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

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ourios_bench::{
    FIXTURE_SERVICE, FIXTURE_SERVICE_B, FIXTURE_TRACE, FixtureRecord, LineKey, LokiFetchedBytes,
    comparative_fixture, compare_lines, fixture_jsonl, fixture_logs_data, ourios_query_lines,
    parse_loki_bytes_processed, parse_loki_fetched_bytes, parse_loki_streams,
};
use ourios_core::tenant::TenantId;

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
            Ok(resp) if resp.status().is_success() => return,
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
#[test]
#[ignore = "RFC0031.2 stub — implemented in the L-gate + bytes-read green slice"]
fn rfc0031_2_l1_template_lookup_bytes() {
    todo!(
        "RFC0031.2 — on the headline OTel-Demo corpus, a template \
         matching <0.1% of lines: ourios.bytes_read / loki.bytes_read \
         <= 1/M_L1 (Ourios row-group bytes read per the RFC 0016 \
         extension; Loki Summary.totalBytesProcessed). must-win: above \
         the ratio flips l1.pass = false and surfaces a pillar-level \
         finding (benchmarks.md §7). Cold + warm latency recorded as \
         corroborating, non-gating"
    );
}

/// Scenario RFC0031.3 — L2 attribute predicate wins on bytes read.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.3 stub — implemented in the L-gate + bytes-read green slice"]
fn rfc0031_3_l2_attribute_predicate_bytes() {
    todo!(
        "RFC0031.3 — headline corpus, predicate severity >= ERROR AND \
         service.name = X over a bounded window, expressed equivalently \
         in both DSLs (RFC0031.1 holding): ourios.bytes_read / \
         loki.bytes_read <= 1/M_L2, same pillar-level escalation on \
         failure (resource-context pruning via promoted columns, \
         RFC 0022)"
    );
}

/// Scenario RFC0031.4 — L3 trace correlation wins on bytes read (OTLP-native).
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.4 stub — implemented in the L-gate + bytes-read green slice"]
fn rfc0031_4_l3_trace_correlation_bytes() {
    todo!(
        "RFC0031.4 — headline corpus, 'every log line for this \
         trace_id', with trace_id NOT a Loki label (high-cardinality, \
         un-labelable per §3.3): ourios.bytes_read / loki.bytes_read <= \
         1/M_L3 (Ourios bloom-filtered promoted column; Loki \
         label-stream scan). must-win — a query Loki's model cannot \
         serve without a full scan, so a loss is among the strongest \
         signals against the thesis"
    );
}

/// Scenario RFC0031.5 — L4 frequency aggregation wins on bytes read (OTLP-native).
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.5 stub — implemented in the L-gate + bytes-read green slice"]
fn rfc0031_5_l4_frequency_aggregation_bytes() {
    todo!(
        "RFC0031.5 — headline corpus, count of one template over time \
         grouped by an extracted param (Ourios: columnar GROUP BY on \
         template_id + a typed param column; Loki: count_over_time with \
         a LogQL pattern/label_format extraction over scanned chunks), \
         RFC0031.1 grouped-count-map equivalence holding: \
         ourios.bytes_read / loki.bytes_read <= 1/M_L4. must-win — the \
         query the template + typed-params pillar exists to serve"
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
#[test]
#[ignore = "RFC0031.7 stub — implemented in the L-gate + reporting green slice"]
fn rfc0031_7_l6_broad_scan_floor() {
    todo!(
        "RFC0031.7 — low-selectivity wide-time-range query, RFC0031.1 \
         holding: ourios.latency_p50 <= F_L6 * loki.latency_p50. \
         Exceeding the floor is a tuning-RFC signal, not a pillar-level \
         escalation"
    );
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

/// Scenario RFC0031.11 — losses published and escalation follows §7.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.11 stub — implemented in the reporting + escalation green slice"]
fn rfc0031_11_losses_published_and_escalation() {
    todo!(
        "RFC0031.11 — every taxonomy class appears in benchmarks.md §9 \
         (wins AND losses) with disposition, both systems' numbers, the \
         corpus, and the hardware tag; an L1/L2/L3/L4 bytes-read loss on \
         the headline OTel-Demo corpus is a pillar-level finding pausing \
         further implementation pending a CLAUDE.md §2 revisit, whereas \
         a must-win latency-only loss with a bytes-read win is a roadmap \
         item"
    );
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

    fn margin_label(self) -> &'static str {
        match self {
            Self::MustWin => "must-win margin",
            Self::Floor => "floor factor",
        }
    }
}

/// One measured query of the indicative run: the equivalent DSL/`LogQL`
/// question, the window both systems answer it over, and the row count
/// both must return exactly.
struct PairSpec {
    label: String,
    /// The pair's §7 margin for the reported gate (`m_l2` for the
    /// severity pair, `f_l6` for the broad time-window slices).
    margin: u64,
    /// Direction the gate is reported under: must-win for the severity
    /// pair, floor for the time-window slices.
    gate: GateKind,
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
/// the L6 floor factor.
fn build_pair_specs(
    pair: &SelectivePair,
    clean_ts: &[u64],
    poison_ts: &[u64],
    trace: Option<&(String, u64)>,
    corpus_now: u64,
    corpus_window: u64,
) -> Vec<PairSpec> {
    let margins = ourios_bench::ComparativeMargins::default();
    let mut specs = vec![PairSpec {
        label: format!(
            "severity, L2 family: service={} severity>={} (text={:?})",
            pair.service, pair.threshold, pair.text
        ),
        margin: margins.m_l2,
        gate: GateKind::MustWin,
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
            gate: GateKind::Floor,
            dsl: format!("service == \"{}\" | limit 5000", pair.service),
            logql: format!("{{service_name=\"{}\"}}", pair.service),
            start,
            end,
            expected_rows: k as u64,
            now: end,
            window: end - start,
        });
    }
    // L3 must-win: an exact trace lookup over the full corpus window.
    // Loki cannot pre-narrow a trace to a stream (`.+` selector +
    // structured-metadata filter = a scan across every service); Ourios
    // compiles it to a trace_id column equality. Skipping when no
    // eligible trace exists is LOUD (stderr + a 3-section report), never
    // silent.
    match trace {
        Some((hex, rows)) => specs.push(PairSpec {
            label: format!("trace correlation, L3 family: trace_id={hex}"),
            margin: ourios_bench::ComparativeMargins::default().m_l3,
            gate: GateKind::MustWin,
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
    specs
}

/// One pair's Loki measurement: poll `query_range` until ingest has
/// caught up to the expected row count (or fail loudly at the deadline).
async fn loki_measure_pair(
    http: &reqwest::Client,
    base: &str,
    spec: &PairSpec,
) -> (Vec<LineKey>, u64, LokiFetchedBytes) {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let (lines, bytes, fetched) =
            loki_query_with_stats(http, base, &spec.logql, spec.start, spec.end).await;
        if lines.len() as u64 >= spec.expected_rows {
            break (lines, bytes, fetched);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "loki returned {} of {} expected rows for [{}] before timeout",
            lines.len(),
            spec.expected_rows,
            spec.label,
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The §7 calibration input: the indicative Ourios-vs-Loki bytes-read
/// comparison on a real corpus (`OURIOS_COMPARATIVE_CORPUS`, fetched by
/// the `comparative-bench` dispatch workflow — the `corpus/otel-demo-v*`
/// releases), measured across a three-point selectivity curve
/// ([`build_pair_specs`]) over one container + one corpus replay.
///
/// **Equivalence is asserted per pair; the bytes gates are REPORTED,
/// not asserted** — the §7 margins are provisional until the maintainer
/// freezes them against exactly these numbers (RFC 0031 §7).
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

    // The (locally-proven) Ourios half, per pair.
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
    let specs = build_pair_specs(
        &pair,
        &clean_ts,
        &poison_ts,
        trace.as_ref(),
        corpus_now,
        corpus_window,
    );
    let ourios: Vec<_> = specs
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
            answer
        })
        .collect();

    // The Loki half: one container (stock + documented ingest-side
    // flags), ONE full-corpus OTLP replay, all pairs measured against it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let loki: Vec<_> = runtime.block_on(async {
        let (_container, base, http) = start_loki(&[
            "-validation.reject-old-samples=false",
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
        ])
        .await;
        push_corpus_to_loki(&http, &base, &corpus_dir).await;
        let mut measured = Vec::with_capacity(specs.len());
        for spec in &specs {
            measured.push(loki_measure_pair(&http, &base, spec).await);
        }
        measured
    });

    // Equivalence gates every measurement (RFC0031.1) — asserted.
    for ((spec, ours), (loki_lines, _, _)) in specs.iter().zip(&ourios).zip(&loki) {
        let outcome = compare_lines(&ours.lines, loki_lines, 8);
        assert!(
            outcome.is_equal(),
            "the two systems' answers must be multiset-identical on [{}]: {outcome:?}",
            spec.label,
        );
    }

    print_indicative_report(&corpus_dir, pair.total_records, &specs, &ourios, &loki);
}

/// The indicative run's report block, one section per pair. Each gate is
/// REPORTED under the pair's provisional §7 margin, and evaluated
/// PRIMARILY on the conservative Loki figure: `totalBytesProcessed` is
/// decompressed engine-side work, which overstates Loki's storage reads
/// by the chunk compression ratio; the storage-side figure (compressed
/// chunk bytes + memory-served head-chunk bytes) is the apples-to-apples
/// counterpart of Ourios's fetched-compressed-Parquet bytes. Both ratios
/// are printed so the §9 entry can carry the honest pair of numbers.
fn print_indicative_report(
    corpus_dir: &std::path::Path,
    total_records: u64,
    specs: &[PairSpec],
    ourios: &[ourios_bench::OuriosAnswer],
    loki: &[(Vec<LineKey>, u64, LokiFetchedBytes)],
) {
    println!("=== RFC 0031 indicative comparative run ===");
    println!("corpus: {} ({total_records} records)", corpus_dir.display());
    for ((spec, ours), (_, loki_processed, loki_fetched)) in specs.iter().zip(ourios).zip(loki) {
        let loki_storage = loki_fetched.compressed_bytes + loki_fetched.head_chunk_bytes;
        let gate_storage = spec
            .gate
            .evaluate(ours.bytes_read, loki_storage, spec.margin);
        let gate_processed = spec
            .gate
            .evaluate(ours.bytes_read, *loki_processed, spec.margin);
        println!("--- pair [{}] rows={} ---", spec.label, spec.expected_rows);
        println!("dsl: {}", spec.dsl);
        // The Ourios figure is the honest TOTAL (§3.6 measurement-fidelity
        // amendment, 2026-07-12): count scan + row materialization +
        // template-registry derivation — the gates below ratio against it.
        println!(
            "ourios bytes_read (compressed, fetched)   = {} \
             (count_scan={} + materialize={} + registry={})",
            ours.bytes_read, ours.count_scan_bytes, ours.materialize_bytes, ours.registry_bytes,
        );
        println!(
            "loki   storage-side bytes (conservative)  = {loki_storage} \
             (compressed={} + head_chunk={})",
            loki_fetched.compressed_bytes, loki_fetched.head_chunk_bytes,
        );
        println!("loki   totalBytesProcessed (decompressed) = {loki_processed}");
        println!(
            "gate vs storage-side (PRIMARY, {} {}): {gate_storage:?}",
            spec.gate.margin_label(),
            spec.margin
        );
        println!(
            "gate vs bytes-processed (context, {} {}): {gate_processed:?}",
            spec.gate.margin_label(),
            spec.margin
        );
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
