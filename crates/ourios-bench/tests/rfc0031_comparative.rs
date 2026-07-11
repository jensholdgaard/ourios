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
    fixture_logs_data, ourios_query_lines, parse_loki_streams,
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
        - 30_000_000_000; // 30 s ago
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

/// The Loki half of RFC0031.1: start a real Loki container, push the SAME
/// `LogsData` value the Ourios corpus was rendered from over the native
/// OTLP endpoint, then answer two `LogQL` queries — the fixture-equivalent
/// one (all lines) and a deliberately narrower one (the mismatch arm).
async fn loki_round_trip(records: &[FixtureRecord], base_ns: u64) -> (Vec<LineKey>, Vec<LineKey>) {
    use prost::Message as _;
    use testcontainers_modules::testcontainers::core::ContainerPort;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;
    use testcontainers_modules::testcontainers::{GenericImage, ImageExt};

    // The stock image config (schema v13 / TSDB) serves the native OTLP
    // endpoint and maps `service.name` → the `service_name` stream label;
    // auth is disabled. Exactly what a competent single-binary operator
    // gets out of the box.
    let container = GenericImage::new(LOKI_IMAGE, LOKI_TAG)
        .with_exposed_port(ContainerPort::Tcp(3100))
        .with_cmd(["-config.file=/etc/loki/local-config.yaml"])
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
        .timeout(Duration::from_secs(10))
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

    // Push the SAME LogsData value the Ourios corpus was rendered from,
    // as the OTLP/HTTP protobuf body Loki's endpoint takes.
    let payload = opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
        resource_logs: fixture_logs_data(records).resource_logs,
    }
    .encode_to_vec();
    let push = http
        .post(format!("{base}/otlp/v1/logs"))
        .header("content-type", "application/x-protobuf")
        .body(payload)
        .send()
        .await
        .expect("otlp push");
    assert!(
        push.status().is_success(),
        "loki otlp push rejected: {} — {}",
        push.status(),
        push.text().await.unwrap_or_default(),
    );

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
            ("limit", "1000"),
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
