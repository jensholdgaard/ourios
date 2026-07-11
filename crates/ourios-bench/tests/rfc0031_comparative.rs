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
    FIXTURE_SERVICE, FixtureRecord, LineKey, comparative_fixture, compare_lines, fixture_jsonl,
    fixture_logs_data, ourios_query_lines, parse_loki_bytes_processed, parse_loki_streams,
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
    let ourios_lines = ourios_query_lines(
        bucket.path(),
        &tenant,
        "severity >= 0 | limit 1000",
        now,
        window,
    )
    .expect("ourios extraction");
    assert_eq!(ourios_lines.len(), 3, "Ourios returns every fixture line");

    // ------------------------------------------------------------------
    // Loki half (async): container → OTLP push → LogQL → LineKeys.
    // ------------------------------------------------------------------
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (loki_all, loki_narrow) = runtime.block_on(loki_round_trip(&records, base_ns));

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
/// OTLP endpoint, then answer two `LogQL` queries — the fixture-equivalent
/// one (all lines) and a deliberately narrower one (the mismatch arm).
async fn loki_round_trip(records: &[FixtureRecord], base_ns: u64) -> (Vec<LineKey>, Vec<LineKey>) {
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
    (loki_all, loki_narrow)
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
/// check is meaningful. Generalised from a hardcoded ERROR band because
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

    // service -> severity_number -> severity_text -> row count
    let mut per_service: HashMap<String, HashMap<i32, HashMap<String, u64>>> = HashMap::new();
    let (mut total, mut min_ts, mut max_ts) = (0u64, u64::MAX, 0u64);

    let mut paths: Vec<_> = std::fs::read_dir(corpus_dir)
        .expect("read corpus dir")
        .filter_map(|e| {
            let p = e.expect("dir entry").path();
            (p.extension().and_then(|x| x.to_str()) == Some("jsonl")).then_some(p)
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no *.jsonl in {}", corpus_dir.display());

    for path in paths {
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
                        // Zero-`time_unix_nano` records are excluded from
                        // candidate bands even though both systems could
                        // still return them (Ourios windows the RFC 0005
                        // §3.2 EFFECTIVE timestamp, falling back to
                        // observed; Loki's OTLP ingest falls back to
                        // observed too): the two sides would answer with
                        // DIFFERENT timestamps — Ourios's row keeps
                        // time_unix_nano = 0, Loki stamps its stored
                        // (observed) time — so their LineKeys can never
                        // match and any band containing such rows is a
                        // guaranteed equivalence mismatch.
                        if lr.time_unix_nano == 0 {
                            continue;
                        }
                        min_ts = min_ts.min(lr.time_unix_nano);
                        max_ts = max_ts.max(lr.time_unix_nano);
                        let texts = entry.entry(lr.severity_number).or_default();
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

/// The candidate `(rows, threshold, service, text)` tuples for
/// [`pick_selective_pair`]: for every service and every DISTINCT severity
/// number T in it (the nested map dedupes them), the rows with
/// `number ≥ T` are a candidate iff (a) they all share ONE text t, (b)
/// the count is `1..=4000`, and (c) — the reverse direction — NO row with
/// text t sits below T, so `LogQL`'s text filter selects exactly the same
/// rows as the DSL's number threshold. No-service (empty-key) records are
/// scanned — they show in the failure diagnostic — but never form
/// candidates (an empty service can't make a valid DSL/LogQL pair).
fn select_pair_candidates(
    per_service: &std::collections::HashMap<
        String,
        std::collections::HashMap<i32, std::collections::HashMap<String, u64>>,
    >,
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
    for (svc, bands) in per_service {
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
            candidates.push((rows, threshold, svc, *text));
        }
    }
    candidates
}

/// One `query_range` call returning both the lines and Loki's
/// bytes-processed measurement from the same response body.
async fn loki_query_with_stats(
    http: &reqwest::Client,
    base: &str,
    logql: &str,
    start: u64,
    end: u64,
) -> (Vec<LineKey>, u64) {
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
    )
}

/// Push a whole OTLP/JSON Lines corpus into Loki, batched by **encoded
/// bytes** (≈3 MiB per request, under Loki's stock 4 MiB internal gRPC
/// message cap) with a 500-`LogsData` secondary cap, via the retrying
/// pusher. Byte-capped because count-capped batching is blind to
/// heterogeneous batch sizes — run #2 died on a 500-batch push that
/// encoded to 5.28 MB (503 ResourceExhausted); adapting the pusher to
/// Loki's stock limit is the anti-strawman direction (a real OTLP
/// exporter batches under size limits too).
async fn push_corpus_to_loki(http: &reqwest::Client, base: &str, corpus_dir: &std::path::Path) {
    use prost::Message as _;
    use std::io::BufRead as _;

    /// Stay ~1 MiB under Loki's stock 4 MiB cap: the encoded-size
    /// estimate below is per-`ResourceLogs` and slightly undercounts the
    /// envelope's field framing.
    const FLUSH_BYTES: usize = 3 * 1024 * 1024;

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
                    eprintln!("loki ingest: {batched} LogsData batches pushed…");
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
    }
    eprintln!("loki ingest complete: {batched} LogsData batches");
}

/// The §7 calibration input: the first indicative Ourios-vs-Loki
/// bytes-read comparison on a real corpus (`OURIOS_COMPARATIVE_CORPUS`,
/// fetched by the `comparative-bench` dispatch workflow — the
/// `corpus/otel-demo-v*` releases).
///
/// **Equivalence is asserted; the bytes gate is REPORTED, not asserted**
/// — the §7 margins are provisional until the maintainer freezes them
/// against exactly this run's numbers (RFC 0031 §7).
///
/// Loki runs the stock image config plus two explicit, documented
/// ingest-side deviations (both in LOKI'S favour — the anti-strawman
/// direction): `-validation.reject-old-samples=false` (the frozen captures carry
/// their original timestamps, weeks old by run time) and raised
/// ingest-rate limits so a 2.96 GB replay isn't throttled by dev-scale
/// defaults. The query side stays stock.
#[test]
#[ignore = "dispatch-only: needs Docker + a corpus via OURIOS_COMPARATIVE_CORPUS (comparative-bench workflow)"]
fn rfc0031_indicative_comparative_run() {
    let corpus_dir = std::path::PathBuf::from(
        std::env::var("OURIOS_COMPARATIVE_CORPUS")
            .expect("set OURIOS_COMPARATIVE_CORPUS to a corpus dir (the dispatch workflow does)"),
    );

    // Pick the pair, then drive the (locally-proven) Ourios half.
    let pair = pick_selective_pair(&corpus_dir);
    eprintln!("pair: {pair:?}");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        &corpus_dir,
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;
    let dsl = format!(
        "service == \"{}\" and severity >= {} | limit 5000",
        pair.service, pair.threshold
    );
    let ourios = ourios_bench::ourios_query_answer(bucket.path(), &tenant, &dsl, now, window)
        .expect("ourios answer");
    assert_eq!(
        ourios.lines.len() as u64,
        pair.rows,
        "Ourios must return exactly the picked pair's rows",
    );

    // The Loki half: container (stock + documented ingest-side flags),
    // full-corpus OTLP replay, the equivalent LogQL.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (loki_lines, loki_bytes) = runtime.block_on(async {
        let (_container, base, http) = start_loki(&[
            "-validation.reject-old-samples=false",
            "-distributor.ingestion-rate-limit-mb=512",
            "-distributor.ingestion-burst-size-mb=1024",
            "-ingester.per-stream-rate-limit=512MB",
            "-ingester.per-stream-rate-limit-burst=1GB",
        ])
        .await;
        push_corpus_to_loki(&http, &base, &corpus_dir).await;

        let logql = format!(
            "{{service_name=\"{}\"}} | severity_text=\"{}\"",
            pair.service, pair.text
        );
        let (start, end) = (pair.min_ts, pair.max_ts + 1);
        // Poll until ingest catches up to the expected row count.
        let deadline = std::time::Instant::now() + Duration::from_secs(300);
        loop {
            let (lines, bytes) = loki_query_with_stats(&http, &base, &logql, start, end).await;
            if lines.len() as u64 >= pair.rows {
                break (lines, bytes);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "loki returned {} of {} expected rows before timeout",
                lines.len(),
                pair.rows,
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    // Equivalence gates the measurement (RFC0031.1) — asserted.
    let outcome = compare_lines(&ourios.lines, &loki_lines, 8);
    assert!(
        outcome.is_equal(),
        "the two systems' answers must be multiset-identical: {outcome:?}",
    );

    // The bytes gate — REPORTED under the provisional §7 margins.
    let margins = ourios_bench::ComparativeMargins::default();
    let gate = ourios_bench::bytes_must_win(ourios.bytes_read, loki_bytes, margins.m_l2);
    println!("=== RFC 0031 indicative comparative run ===");
    println!(
        "corpus: {} ({} records)",
        corpus_dir.display(),
        pair.total_records
    );
    println!(
        "pair (L2 family): service={} severity>={} (text={:?}) rows={}",
        pair.service, pair.threshold, pair.text, pair.rows
    );
    println!("ourios bytes_read      = {}", ourios.bytes_read);
    println!("loki   bytes_processed = {loki_bytes}");
    println!("gate (provisional margin {}): {gate:?}", margins.m_l2);
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
    assert_eq!(pair.service, FIXTURE_SERVICE);
    assert_eq!(
        pair.threshold, 17,
        "the ERROR band is the rarest single-text band"
    );
    assert_eq!(pair.text, "ERROR");
    assert_eq!(pair.rows, 1, "exactly the one ERROR fixture record");
    assert_eq!(pair.total_records, 3);
    assert_eq!(pair.min_ts, 1_000_000);
    assert_eq!(pair.max_ts, 1_002_000);
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
        },
        FixtureRecord {
            time_unix_nano: 2_000,
            severity_number: 9,
            severity_text: "INFO",
            body: "user 2 logged in",
        },
        FixtureRecord {
            time_unix_nano: 3_000,
            severity_number: 13,
            severity_text: "WARN",
            body: "cache nearly full",
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
